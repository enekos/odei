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
        // Command rules match on first-token prefix: "cargo" allows any cargo …
        target.trim().starts_with(rule.target.trim())
    } else {
        target == rule.target
    }
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
    fn remembered_rule_matches_command_prefix() {
        let mut rules = RuleStore::default();
        rules.rules.push(Rule {
            id: "rule-1".into(),
            effect: "allow".into(),
            tool: "terminal".into(),
            target: "git".into(),
        });
        let decision = classify(
            &ctx(),
            &rules,
            PermissionMode::Ask,
            spec("terminal"),
            &json!({"action": "exec", "command": "git push origin main"}),
        );
        assert_eq!(decision, Decision::Allow);
    }
}
