//! Tool-call inspection: pick a call, read what it really did.
//!
//! `/calls` draws a short picker of this session's tool calls and lets you
//! click one; `/call N` jumps straight to `#N`. Either way the full account —
//! the command that reproduces it, the arguments, and the complete
//! untruncated output — is rendered to a file and opened in a side pane.
//!
//! Two deliberate constraints shape this:
//!
//! * **Mouse reporting is confined to the picker.** While it is on, the
//!   terminal sends clicks to us instead of selecting text, and rustyline
//!   would read those escapes as keystrokes. So it is armed when the picker
//!   draws and disarmed before anything else runs — everywhere else, drag to
//!   select works exactly as it did.
//! * **The pane is the terminal's, not ours.** Under tmux, WezTerm, kitty or
//!   Zellij the report opens in a real split, so scrollback, search and
//!   copy are the ones already in your fingers. Without a multiplexer it
//!   opens in the pager, and the file path is printed either way.

use crate::calls::{self, Record};
use crate::theme::Theme;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use std::path::{Path, PathBuf};

/// Most calls offered at once; the rest stay reachable by `/call N`.
const PICKER_CAP: usize = 24;

/// Screen row of the i-th offered call, counted back from where the cursor
/// ended up after drawing. Going backwards from the end is what makes this
/// survive the screen scrolling mid-draw: the layout below the rows is fixed
/// (one footer line, then the cursor), while the rows above may have moved.
fn row_of(end_row: u16, count: usize, i: usize) -> u16 {
    let from_bottom = (count - i) as u16 + 1;
    end_row.saturating_sub(from_bottom)
}

/// Terminal size, with a floor: a pty that was never given a window size
/// reports 0×0, and every width calculation downstream would collapse.
fn size() -> (u16, u16) {
    let (width, height) = crossterm::terminal::size().unwrap_or((0, 0));
    (
        if width < 20 { 80 } else { width },
        if height < 6 { 24 } else { height },
    )
}

/// Where the drawn block ended, if the terminal answered the cursor-position
/// query with something consistent with what we just printed.
///
/// Without an answer we must not address rows at all: absolute positioning
/// would land at the top of the screen and overwrite whatever is there, and
/// clicks would map to the wrong calls. `None` means "degrade".
fn addressable_end_row(count: usize) -> Option<u16> {
    let row = crossterm::cursor::position().ok().map(|(_, row)| row)?;
    // The block is a header, `count` rows, and a footer above the cursor;
    // any smaller row than that means nothing answered us.
    (row >= count as u16 + 2).then_some(row)
}

/// Cut to the terminal width so a printed row never wraps — the picker maps
/// screen rows to calls by counting lines, and a wrapped line breaks that.
fn fit(text: &str, width: u16) -> String {
    let cap = (width as usize).saturating_sub(2).max(20);
    if text.chars().count() <= cap {
        text.to_string()
    } else {
        text.chars().take(cap - 1).collect::<String>() + "…"
    }
}

// ------------------------------------------------------------------- pane

/// A terminal that can split itself, discovered from the environment rather
/// than configured. Each variant is only chosen when its own env var proves
/// we are inside it with control available.
enum Splitter {
    Tmux,
    WezTerm,
    Kitty,
    Zellij,
    None,
}

fn splitter() -> Splitter {
    let has = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if has("TMUX") {
        Splitter::Tmux
    } else if has("ZELLIJ") {
        Splitter::Zellij
    } else if has("WEZTERM_PANE") {
        Splitter::WezTerm
    } else if has("KITTY_LISTEN_ON") {
        // Only set when remote control is actually enabled.
        Splitter::Kitty
    } else {
        Splitter::None
    }
}

/// `less` if we have it, honouring $PAGER. `-R` keeps colour, `-S` stops
/// long output lines from wrapping into each other.
fn pager(quit_if_short: bool) -> Vec<String> {
    let configured = std::env::var("PAGER").ok().filter(|p| !p.trim().is_empty());
    match configured {
        Some(pager) => pager.split_whitespace().map(str::to_string).collect(),
        None => {
            let mut flags = vec!["less".to_string(), "-R".into(), "-S".into()];
            if quit_if_short {
                // Short reports shouldn't demand a keypress; -X leaves them
                // on screen after less exits.
                flags.push("-F".into());
                flags.push("-X".into());
            }
            flags
        }
    }
}

fn spawn(program: &str, args: &[String]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The viewer as one shell string, for splitters that hand their tail to
/// `sh -c` rather than exec'ing an argv (tmux does; the others take `--`).
fn shell_line(command: &[String]) -> String {
    command
        .iter()
        .map(|part| crate::calls::quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open `path` beside the current pane. Returns how it was opened, for the
/// one-line note printed to the user.
fn open_pane(path: &Path) -> &'static str {
    let file = path.display().to_string();
    let mut view = pager(false);
    let pager_program = view.remove(0);
    let mut command: Vec<String> = vec![pager_program.clone()];
    command.extend(view);
    command.push(file.clone());

    match splitter() {
        Splitter::Tmux => {
            // One string: tmux would otherwise read the pager's own flags as
            // split-window options.
            let args = vec![
                "split-window".to_string(),
                "-h".into(),
                shell_line(&command),
            ];
            if spawn("tmux", &args) {
                return "tmux split";
            }
        }
        Splitter::Zellij => {
            let mut args = vec![
                "action".to_string(),
                "new-pane".into(),
                "--direction".into(),
                "right".into(),
                "--".into(),
            ];
            args.extend(command.clone());
            if spawn("zellij", &args) {
                return "zellij pane";
            }
        }
        Splitter::WezTerm => {
            let mut args = vec![
                "cli".to_string(),
                "split-pane".into(),
                "--right".into(),
                "--percent".into(),
                "45".into(),
                "--".into(),
            ];
            args.extend(command.clone());
            if spawn("wezterm", &args) {
                return "wezterm pane";
            }
        }
        Splitter::Kitty => {
            let mut args = vec![
                "@".to_string(),
                "launch".into(),
                "--location=vsplit".into(),
                "--cwd=current".into(),
            ];
            args.extend(command.clone());
            if spawn("kitty", &args) {
                return "kitty split";
            }
        }
        Splitter::None => {}
    }

    // No multiplexer, or the split failed: page it here instead.
    let mut view = pager(true);
    let program = view.remove(0);
    view.push(file);
    if spawn(&program, &view) {
        "pager"
    } else {
        "file"
    }
}

fn report_path(session_id: &str, n: usize) -> PathBuf {
    let dir = crate::config::odei_home().join("calls");
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    dir.join(format!("{safe}-call-{n}.txt"))
}

/// Render `#n` and open it. Returns false if there is no such call.
pub fn show(theme: &Theme, session_id: &str, n: usize) -> bool {
    let records = calls::load(session_id);
    let Some(record) = records.iter().find(|r| r.n == n) else {
        let known = records.last().map(|r| r.n).unwrap_or(0);
        if known == 0 {
            println!(
                "{}no tool calls in this session yet{}",
                theme.dim,
                theme.reset()
            );
        } else {
            println!(
                "{}no call #{n} in this session (calls are #1–#{known}){}",
                theme.dim,
                theme.reset()
            );
        }
        return false;
    };
    show_record(theme, session_id, record);
    true
}

fn show_record(theme: &Theme, session_id: &str, record: &Record) {
    let (width, _) = size();
    // A split takes roughly half of what we can see.
    let measure = match splitter() {
        Splitter::None => width as usize,
        _ => (width / 2) as usize,
    };
    let text = calls::report(record, measure);
    let path = report_path(session_id, record.n);
    if let Err(e) = std::fs::write(&path, &text) {
        println!(
            "{}cannot write the report: {e}{}",
            theme.warning,
            theme.reset()
        );
        return;
    }
    let how = open_pane(&path);
    println!(
        "{}call #{} in {how} · {}{}",
        theme.dim,
        record.n,
        path.display(),
        theme.reset()
    );
}

// ----------------------------------------------------------------- picker

/// Draw the recent calls, let one be clicked (or arrowed to, or typed by
/// number), and open it. Falls back to a plain list when the terminal cannot
/// be driven.
pub fn picker(theme: &Theme, session_id: &str) {
    let records = calls::load(session_id);
    if records.is_empty() {
        println!(
            "{}no tool calls in this session yet{}",
            theme.dim,
            theme.reset()
        );
        return;
    }
    let (width, height) = size();
    let room = (height as usize).saturating_sub(6).max(3);
    let show_count = records.len().min(PICKER_CAP).min(room);
    let offered = &records[records.len() - show_count..];

    if crossterm::terminal::enable_raw_mode().is_err() {
        // No terminal control: print the list and let /call N do the rest.
        for record in offered {
            println!(
                "{}{}{}",
                theme.dim,
                fit(&calls::summary_line(record), width),
                theme.reset()
            );
        }
        println!("{}open one with /call N{}", theme.dim, theme.reset());
        return;
    }

    let hidden = records.len() - show_count;
    let header = if hidden > 0 {
        format!("● {show_count} recent tool calls ({hidden} earlier — /call N)")
    } else {
        format!(
            "● {show_count} tool call{}",
            if show_count == 1 { "" } else { "s" }
        )
    };
    let footer = "click a call · ↑↓ then ⏎ · type a number · esc to leave";

    // Printed before the rows so the row arithmetic below can find them.
    print!(
        "\r\n{}{}{}\r\n",
        theme.dim,
        fit(&header, width),
        theme.reset()
    );
    for record in offered {
        print!(
            "{}  {}{}\r\n",
            theme.dim,
            fit(&calls::summary_line(record), width),
            theme.reset()
        );
    }
    print!("{}{}{}\r\n", theme.dim, fit(footer, width), theme.reset());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Highlighting and clicking both need to know which screen row each call
    // landed on. If the terminal won't say, the list stays as printed and
    // only the keyboard drives it — better than drawing over the screen.
    let Some(end_row) = addressable_end_row(show_count) else {
        // Replace the footer we just printed — it promises clicking, which
        // this terminal has turned out not to support. One line up is a
        // relative move, so it needs none of the row arithmetic below.
        print!(
            "\x1b[1A\r\x1b[2K{}open one with /call N{}\r\n",
            theme.dim,
            theme.reset()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let _ = crossterm::terminal::disable_raw_mode();
        return;
    };
    let row_of = |i: usize| row_of(end_row, show_count, i);
    let first_row = row_of(0).saturating_sub(1);

    let mut stdout = std::io::stdout();
    let mouse = execute!(stdout, EnableMouseCapture).is_ok();
    let mut selected = show_count - 1;
    let mut typed = String::new();

    let draw = |i: usize, active: bool| {
        let record = &offered[i];
        let style = if active {
            theme.selected_completion
        } else {
            theme.dim
        };
        let marker = if active { '›' } else { ' ' };
        // Absolute placement, so redrawing never disturbs anything else.
        print!(
            "\x1b[{};1H\x1b[2K{style}{marker} {}{}",
            row_of(i) + 1,
            fit(&calls::summary_line(record), width),
            theme.reset()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };
    draw(selected, true);

    let chosen = loop {
        match crossterm::event::read() {
            Ok(Event::Mouse(event)) => {
                if event.kind == MouseEventKind::Down(MouseButton::Left) {
                    if let Some(i) = (0..show_count).find(|&i| row_of(i) == event.row) {
                        break Some(i);
                    }
                }
            }
            Ok(Event::Key(key)) => match key.code {
                KeyCode::Up | KeyCode::Char('k') if selected > 0 => {
                    draw(selected, false);
                    selected -= 1;
                    draw(selected, true);
                }
                KeyCode::Down | KeyCode::Char('j') if selected + 1 < show_count => {
                    draw(selected, false);
                    selected += 1;
                    draw(selected, true);
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    typed.push(c);
                    // A typed number addresses a call by its #N, which may be
                    // one the picker isn't showing.
                    if let Some(i) = typed
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| offered.iter().position(|r| r.n == n))
                    {
                        draw(selected, false);
                        selected = i;
                        draw(selected, true);
                    }
                }
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Enter => break Some(selected),
                KeyCode::Esc | KeyCode::Char('q') => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };

    if mouse {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    let _ = crossterm::terminal::disable_raw_mode();
    // Take the picker back off the screen: it has served its purpose, and the
    // report is about to arrive.
    print!("\x1b[{};1H\x1b[J", first_row + 1);
    let _ = std::io::Write::flush(&mut stdout);

    match chosen {
        Some(i) => show_record(theme, session_id, &offered[i]),
        None => {
            // A number typed for a call outside the window still counts.
            match typed.parse::<usize>() {
                Ok(n) if n > 0 => {
                    show(theme, session_id, n);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_never_exceeds_the_width() {
        let long = "x".repeat(200);
        assert!(fit(&long, 40).chars().count() <= 39);
        assert!(fit(&long, 40).ends_with('…'));
        assert_eq!(fit("short", 80), "short");
        // Absurdly narrow terminals still produce something printable.
        assert!(!fit(&long, 1).is_empty());
    }

    #[test]
    fn pager_honours_the_environment_and_flags_short_reports() {
        // Default: less with colour and no line wrapping.
        std::env::remove_var("PAGER");
        let paged = pager(false);
        assert_eq!(paged[0], "less");
        assert!(paged.contains(&"-R".to_string()));
        assert!(
            !paged.contains(&"-F".to_string()),
            "a split pane must not self-close"
        );
        assert!(
            pager(true).contains(&"-F".to_string()),
            "inline paging should quit if short"
        );

        std::env::set_var("PAGER", "bat --plain");
        assert_eq!(pager(false), vec!["bat", "--plain"]);
        std::env::remove_var("PAGER");
    }

    #[test]
    fn picker_rows_map_back_from_the_cursor() {
        // Drawn: header, 3 rows, footer — cursor lands on row 10, so the
        // footer is 9, the last row 8, the first 6, and the header 5.
        assert_eq!(row_of(10, 3, 2), 8);
        assert_eq!(row_of(10, 3, 1), 7);
        assert_eq!(row_of(10, 3, 0), 6);
        // Rows are contiguous and strictly below the header.
        let rows: Vec<u16> = (0..3).map(|i| row_of(10, 3, i)).collect();
        assert!(rows.windows(2).all(|w| w[1] == w[0] + 1));
        // A click below the last row or on the footer matches nothing.
        assert!(!(0..3).any(|i| row_of(10, 3, i) == 9));
        // Near the top of the screen it saturates instead of wrapping around.
        assert_eq!(row_of(1, 3, 0), 0);
    }

    #[test]
    fn tmux_gets_one_quoted_string_not_loose_flags() {
        let line = shell_line(&["less".into(), "-R".into(), "/tmp/a b/call-1.txt".into()]);
        assert_eq!(line, "less -R '/tmp/a b/call-1.txt'");
    }

    #[test]
    fn report_paths_are_confined_to_the_profile() {
        let path = report_path("../../etc/passwd", 3);
        assert!(path.starts_with(crate::config::odei_home().join("calls")));
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "etcpasswd-call-3.txt"
        );
    }
}
