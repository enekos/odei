use crate::config::odei_home;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Project,
    Personal,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Personal => "personal",
        }
    }
}

pub struct UserCommand {
    pub name: String,
    pub description: Option<String>,
    pub scope: Scope,
    pub body: String,
}

pub fn search_paths(workspace: &Path) -> Vec<(Scope, PathBuf)> {
    let project = workspace.join(".odei").join("commands");
    let personal = odei_home().join("commands");
    let mut paths = vec![(Scope::Project, project.clone())];
    if personal != project {
        paths.push((Scope::Personal, personal));
    }
    paths
}

pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn find(workspace: &Path, name: &str) -> Option<UserCommand> {
    if !is_valid_name(name) {
        return None;
    }
    for (scope, dir) in search_paths(workspace) {
        let path = dir.join(format!("{name}.md"));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (description, body) = split_front_matter(&text);
        if body.trim().is_empty() {
            continue;
        }
        return Some(UserCommand { name: name.to_string(), description, scope, body });
    }
    None
}

pub fn list(workspace: &Path) -> Vec<UserCommand> {
    let mut found: Vec<UserCommand> = Vec::new();
    for (scope, dir) in search_paths(workspace) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()? != "md" {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.to_string();
                is_valid_name(&name).then_some(name)
            })
            .collect();
        names.sort();
        for name in names {
            if found.iter().any(|command| command.name == name) {
                continue;
            }
            if let Some(command) = find(workspace, &name) {
                if command.scope == scope {
                    found.push(command);
                }
            }
        }
    }
    found
}

fn split_front_matter(text: &str) -> (Option<String>, String) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text.to_string());
    };
    let (front, after) = rest.split_at(end);
    let body = after.trim_start_matches("\n---").trim_start_matches('\n');
    let description = front.lines().find_map(|line| {
        let value = line.trim().strip_prefix("description:")?.trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    });
    (description, body.to_string())
}

pub fn expand(body: &str, arguments: &str) -> String {
    let arguments = arguments.trim();
    let words: Vec<&str> = arguments.split_whitespace().collect();
    let mut substituted = body.contains("$ARGUMENTS");
    let mut out = body.replace("$ARGUMENTS", arguments);
    for index in 1..=9usize {
        let token = format!("${index}");
        if out.contains(&token) {
            substituted = true;
            out = out.replace(&token, words.get(index - 1).copied().unwrap_or(""));
        }
    }
    if !substituted && !arguments.is_empty() {
        out.push_str("\n\n");
        out.push_str(arguments);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odei-test-cmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".odei").join("commands")).unwrap();
        dir
    }

    fn write(workspace: &Path, name: &str, text: &str) {
        std::fs::write(workspace.join(".odei").join("commands").join(name), text).unwrap();
    }

    #[test]
    fn a_command_file_is_a_prompt_with_optional_front_matter() {
        let workspace = tempdir("front");
        write(
            &workspace,
            "review.md",
            "---\ndescription: review the diff\nmodel: k3\n---\n\nReview the working tree.\n",
        );
        let command = find(&workspace, "review").unwrap();
        assert_eq!(command.description.as_deref(), Some("review the diff"));
        assert_eq!(command.body.trim(), "Review the working tree.");
        assert_eq!(command.scope, Scope::Project);

        write(&workspace, "bare.md", "Just a prompt.\n");
        let bare = find(&workspace, "bare").unwrap();
        assert_eq!(bare.description, None);
        assert_eq!(bare.body.trim(), "Just a prompt.");

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn arguments_fill_placeholders_or_are_appended_when_there_are_none() {
        assert_eq!(expand("Explain $ARGUMENTS please", "src/ui.rs"), "Explain src/ui.rs please");
        assert_eq!(expand("Compare $1 with $2", "a.rs b.rs"), "Compare a.rs with b.rs");
        assert_eq!(expand("Compare $1 with $2", "a.rs"), "Compare a.rs with");
        assert_eq!(expand("Run the tests", "in the ui crate"), "Run the tests\n\nin the ui crate");
        assert_eq!(expand("Run the tests", ""), "Run the tests");
    }

    #[test]
    fn a_name_that_is_not_a_plain_word_never_reaches_the_filesystem() {
        let workspace = tempdir("name");
        assert!(find(&workspace, "../../etc/passwd").is_none());
        assert!(find(&workspace, "a/b").is_none());
        assert!(find(&workspace, "").is_none());
        assert!(is_valid_name("deploy-check_2"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn listing_reports_every_project_command_once() {
        let workspace = tempdir("list");
        write(&workspace, "b.md", "second");
        write(&workspace, "a.md", "---\ndescription: first\n---\nfirst");
        write(&workspace, "notes.txt", "ignored");
        let listed = list(&workspace);
        let names: Vec<&str> = listed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(listed[0].description.as_deref(), Some("first"));
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
