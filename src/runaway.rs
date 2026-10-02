const WINDOW: usize = 4096;
const MAX_PERIOD: usize = 512;
const CHECK_EVERY: usize = 256;

#[derive(Debug, Default)]
pub struct RepetitionGuard {
    next_check: usize,
}

impl RepetitionGuard {
    pub fn runaway(&mut self, buffer: &str) -> Option<usize> {
        if buffer.len() < self.next_check.max(WINDOW) {
            return None;
        }
        self.next_check = buffer.len() + CHECK_EVERY;
        repeating_tail(buffer.as_bytes())
    }
}

pub fn repeating_tail(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < WINDOW {
        return None;
    }
    let window_start = bytes.len() - WINDOW;
    (1..=MAX_PERIOD).find(|&period| {
        (window_start + period..bytes.len())
            .rev()
            .all(|i| bytes[i] == bytes[i - period])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logged_kimi_runaway_is_caught() {
        let mut args = String::from(
            r#"{"command":"git status","timeout_ms":30000,"profile":"user","session_id":"na","include"#,
        );
        while args.len() < 20_000 {
            args.push_str("_default");
        }
        assert_eq!(repeating_tail(args.as_bytes()), Some(8));
    }

    #[test]
    fn ordinary_code_and_prose_pass() {
        let source = include_str!("agent.rs");
        let mut guard = RepetitionGuard::default();
        for end in (0..source.len()).step_by(97) {
            if !source.is_char_boundary(end) {
                continue;
            }
            assert_eq!(
                guard.runaway(&source[..end]),
                None,
                "false positive at {end}"
            );
        }
    }

    #[test]
    fn short_repeated_runs_are_allowed() {
        let mut text = "fn main() {}\n".repeat(300);
        text.push_str(&"=".repeat(200));
        text.push_str(&"| a | b |\n".repeat(200));
        text.push_str("done");
        assert_eq!(repeating_tail(text.as_bytes()), None);
    }

    #[test]
    fn a_single_character_flood_is_caught() {
        let text = format!("answer: {}", "a".repeat(WINDOW + 10));
        assert_eq!(repeating_tail(text.as_bytes()), Some(1));
    }

    #[test]
    fn the_guard_only_rescans_after_growth() {
        let mut guard = RepetitionGuard::default();
        let flood = "xy".repeat(WINDOW);
        assert_eq!(guard.runaway(&flood), Some(2));
        assert_eq!(guard.runaway(&flood), None);
        let longer = format!("{flood}{}", "xy".repeat(CHECK_EVERY));
        assert_eq!(guard.runaway(&longer), Some(2));
    }
}
