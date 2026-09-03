use crate::commands;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::path::{Path, PathBuf};

const CANDIDATE_CAP: usize = 100;

pub struct OdeiHelper {
    workspace: PathBuf,
    builtins: Vec<&'static str>,
}

impl OdeiHelper {
    pub fn new(workspace: &Path, builtins: Vec<&'static str>) -> OdeiHelper {
        OdeiHelper {
            workspace: workspace.to_path_buf(),
            builtins,
        }
    }

    fn command_candidates(&self, prefix: &str) -> Vec<Pair> {
        let mut names: Vec<String> = self
            .builtins
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| name.to_string())
            .collect();
        for command in commands::list(&self.workspace) {
            if command.name.starts_with(prefix) && !names.contains(&command.name) {
                names.push(command.name);
            }
        }
        names.sort();
        names
            .into_iter()
            .take(CANDIDATE_CAP)
            .map(|name| Pair {
                display: format!("/{name}"),
                replacement: format!("/{name} "),
            })
            .collect()
    }

    fn directory_for(&self, typed: &str) -> PathBuf {
        if typed.is_empty() {
            return self.workspace.clone();
        }
        if let Some(rest) = typed.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        let path = PathBuf::from(typed);
        if path.is_absolute() {
            path
        } else {
            self.workspace.join(path)
        }
    }

    fn path_candidates(&self, prefix: &str) -> Vec<Pair> {
        let (typed_dir, name_prefix) = match prefix.rfind('/') {
            Some(slash) => (&prefix[..=slash], &prefix[slash + 1..]),
            None => ("", prefix),
        };
        let Ok(entries) = std::fs::read_dir(self.directory_for(typed_dir)) else {
            return Vec::new();
        };
        let mut rows: Vec<(bool, String)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                if !name.starts_with(name_prefix) {
                    return None;
                }
                if name.starts_with('.') && !name_prefix.starts_with('.') {
                    return None;
                }
                let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                Some((is_dir, name))
            })
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        rows.into_iter()
            .take(CANDIDATE_CAP)
            .map(|(is_dir, name)| {
                let tail = if is_dir { "/" } else { " " };
                Pair {
                    display: format!("{name}{}", if is_dir { "/" } else { "" }),
                    replacement: format!("@{typed_dir}{name}{tail}"),
                }
            })
            .collect()
    }
}

impl Completer for OdeiHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        let token = &head[start..];
        if let Some(prefix) = token.strip_prefix('/') {
            if start == 0 {
                return Ok((start, self.command_candidates(prefix)));
            }
            return Ok((pos, Vec::new()));
        }
        if let Some(prefix) = token.strip_prefix('@') {
            return Ok((start, self.path_candidates(prefix)));
        }
        Ok((pos, Vec::new()))
    }
}

impl Hinter for OdeiHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for OdeiHelper {}

impl Validator for OdeiHelper {}

impl Helper for OdeiHelper {}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;

    fn candidates(helper: &OdeiHelper, line: &str) -> Vec<String> {
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (_, pairs) = helper.complete(line, line.len(), &ctx).unwrap();
        pairs.into_iter().map(|pair| pair.replacement).collect()
    }

    #[test]
    fn a_slash_completes_commands_and_only_at_the_start_of_the_line() {
        let dir = std::env::temp_dir().join(format!("odei-test-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".odei").join("commands")).unwrap();
        std::fs::write(
            dir.join(".odei").join("commands").join("ship.md"),
            "ship it",
        )
        .unwrap();
        let helper = OdeiHelper::new(&dir, vec!["compact", "copy", "quit"]);

        assert_eq!(candidates(&helper, "/co"), vec!["/compact ", "/copy "]);
        assert_eq!(candidates(&helper, "/sh"), vec!["/ship "]);
        assert!(candidates(&helper, "tell me about /co").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_at_completes_paths_with_directories_first() {
        let dir =
            std::env::temp_dir().join(format!("odei-test-complete-at-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.join("setup.md"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let helper = OdeiHelper::new(&dir, Vec::new());

        assert_eq!(candidates(&helper, "read @s"), vec!["@src/", "@setup.md "]);
        assert_eq!(candidates(&helper, "read @src/"), vec!["@src/main.rs "]);
        assert_eq!(candidates(&helper, "read @."), vec!["@.hidden "]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
