//! Debug trace for diagnosing stuck or slow turns.
//!
//! `ODEI_DEBUG=1` appends timestamped events — requests, retries, stream
//! silence, tool runtimes, approvals, cancels — to `~/.odei/debug/<date>.log`.
//! Set it to a path to log somewhere else. Prompt text and API keys are never
//! logged; sizes and timings are. Logs are pruned after 7 days, like the call
//! journal.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The raw ODEI_DEBUG value, or None when debugging is off.
fn setting() -> Option<&'static str> {
    static SETTING: OnceLock<Option<String>> = OnceLock::new();
    SETTING
        .get_or_init(|| {
            std::env::var("ODEI_DEBUG").ok().filter(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "" | "0" | "off" | "false" | "no"
                )
            })
        })
        .as_deref()
}

pub fn enabled() -> bool {
    setting().is_some()
}

fn log_path() -> PathBuf {
    match setting() {
        Some("1") | Some("true") | Some("on") | Some("yes") => {
            let date = chrono::Local::now().format("%Y-%m-%d");
            crate::config::odei_home()
                .join("debug")
                .join(format!("{date}.log"))
        }
        Some(path) => PathBuf::from(path),
        None => unreachable!("log_path called with debugging off"),
    }
}

fn file() -> Option<&'static Mutex<Option<std::fs::File>>> {
    static FILE: OnceLock<Option<Mutex<Option<std::fs::File>>>> = OnceLock::new();
    FILE.get_or_init(|| {
        if !enabled() {
            return None;
        }
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        prune();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| Mutex::new(Some(f)))
    })
    .as_ref()
}

/// Drop debug logs older than a week, same policy as calls/ and tool-results/.
fn prune() {
    let dir = crate::config::odei_home().join("debug");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let week = std::time::Duration::from_secs(7 * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() > week)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The session id stamped on every line, so interleaved runs stay separable.
fn context() -> &'static Mutex<String> {
    static CONTEXT: OnceLock<Mutex<String>> = OnceLock::new();
    CONTEXT.get_or_init(|| Mutex::new(String::new()))
}

pub fn set_context(id: &str) {
    if let Ok(mut guard) = context().lock() {
        *guard = id.to_string();
    }
}

#[doc(hidden)]
pub fn write_line(message: std::fmt::Arguments) {
    let Some(file) = file() else { return };
    let Ok(mut guard) = file.lock() else { return };
    let Some(file) = guard.as_mut() else { return };
    let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let context = context()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let _ = writeln!(file, "{stamp} [{context}] {message}");
    let _ = file.flush();
}

/// Say where the trace is going, once per process, so a user who turns
/// debugging on knows where to look when something sticks.
pub fn announce() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if enabled() {
            eprintln!("odei: debug log → {}", log_path().display());
        }
    });
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            $crate::debug::write_line(format_args!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn debugging_is_off_by_default() {
        // The test process does not set ODEI_DEBUG.
        assert!(!super::enabled());
    }
}
