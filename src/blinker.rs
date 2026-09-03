//! A subtle activity blinker. While the model thinks, a single dot pulses
//! dimly just under the `⏺ Thinking…` line. It is intentionally small: one
//! row, one cell, and it loops until the first real output arrives.
//!
//! Rendering happens on a spawned thread so the agent loop is never blocked
//! by `sleep`. The sink owns the blinker: it clears the line before printing
//! anything else, and the `Drop` makes sure a forgotten clear can never leave
//! a frozen dot on the screen. `NO_COLOR`, a piped stdout, or a non-interactive
//! run means no blinker at all.

use crate::theme::{self, Theme};
use std::io::Write as _;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// The fixed glyph. Its brightness is carried by the grayscale ramp, so the
/// animation is a gentle fade rather than a strobing shape change.
const GLYPH: char = '·';

/// Frames per second. The blinker should breathe, not strobe.
const FRAME: Duration = Duration::from_millis(120);
/// Height of the blinker block, in terminal rows.
pub const ROWS: usize = 1;
/// The pulse wraps over this many frames, so the loop is seamless.
const CYCLE_FRAMES: u64 = 40;

pub struct Blinker {
    stop: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Blinker {
    /// Reserve the line on screen and start the pulse. `Theme` is a static,
    /// so handing it to the render thread costs nothing.
    pub fn start(theme: &'static Theme) -> Blinker {
        // Reserve ROWS lines, then put the cursor back at the top of the
        // block. Everything the render thread draws lives inside it.
        let mut out = std::io::stdout().lock();
        for _ in 0..ROWS {
            let _ = writeln!(out);
        }
        let _ = write!(out, "\x1b[{ROWS}A\x1b[?25l");
        let _ = out.flush();

        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || run(theme, rx));
        Blinker {
            stop: Some(tx),
            join: Some(join),
        }
    }

    /// Start a blinker only where one can draw. Non-interactive runs and
    /// unstyled themes get `None`.
    pub fn maybe_start(theme: &'static Theme, interactive: bool) -> Option<Blinker> {
        (interactive && theme.enabled).then(|| Blinker::start(theme))
    }

    /// Erase the line and stop the thread. After this the cursor sits on the
    /// row where the block began, ready for ordinary output. Idempotent.
    pub fn clear(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
            let mut out = std::io::stdout().lock();
            for _ in 0..ROWS {
                let _ = writeln!(out, "\x1b[2K");
            }
            let _ = write!(out, "\x1b[{ROWS}A\x1b[2K\x1b[?25h");
            let _ = out.flush();
        }
    }
}

impl Drop for Blinker {
    fn drop(&mut self) {
        self.clear();
    }
}

fn run(theme: &'static Theme, stop: mpsc::Receiver<()>) {
    let ramp = theme::mist_ramp(theme);
    let mut tick: u64 = 0;
    loop {
        match stop.recv_timeout(FRAME) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let mut out = std::io::stdout().lock();
        for line in frame(theme, tick, ramp) {
            let _ = writeln!(out, "{line}\x1b[K");
        }
        // Back to the top of the block, ready for the next frame.
        let _ = write!(out, "\x1b[{ROWS}A");
        let _ = out.flush();
        tick += 1;
    }
}

/// One frame of the blinker: `ROWS` strings, one per row.
fn frame(theme: &Theme, tick: u64, ramp: &[&str; 7]) -> Vec<String> {
    let t = (tick % CYCLE_FRAMES) as f32 / CYCLE_FRAMES as f32;
    // Triangle wave: 0 at the loop ends, 1 in the middle, so the dot fades
    // in and out smoothly.
    let density = 1.0 - (t * 2.0 - 1.0).abs();
    let level = level(density, ramp);
    let line = format!("  {}{}{}", ramp[level], GLYPH, theme.reset());
    vec![line]
}

fn level(density: f32, ramp: &[&str]) -> usize {
    let steps = (ramp.len() - 1) as f32;
    (density.clamp(0.0, 1.0) * steps).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_the_right_height_and_plain_theme_has_no_escapes() {
        let theme = crate::theme::plain();
        let ramp = theme::mist_ramp(theme);
        for tick in 0..10 {
            let lines = frame(theme, tick, ramp);
            assert_eq!(lines.len(), ROWS);
            for line in lines {
                assert!(!line.contains('\x1b'), "{line:?}");
            }
        }
    }

    #[test]
    fn the_blinker_pulses() {
        let theme = crate::theme::dark();
        let ramp = theme::mist_ramp(theme);
        assert_ne!(
            frame(theme, 0, ramp),
            frame(theme, CYCLE_FRAMES / 2, ramp),
            "the blinker should pulse"
        );
    }

    #[test]
    fn the_cycle_wraps_seamlessly() {
        let theme = crate::theme::plain();
        let ramp = theme::mist_ramp(theme);
        assert_eq!(
            frame(theme, 0, ramp),
            frame(theme, CYCLE_FRAMES, ramp),
            "the loop should land back on the first frame"
        );
    }

    #[test]
    fn the_blinker_is_never_all_air() {
        // A pulse that vanishes completely reads as a flicker, not breathing.
        let theme = crate::theme::plain();
        let ramp = theme::mist_ramp(theme);
        let mut saw_ink = false;
        for tick in 0..CYCLE_FRAMES {
            let ink: usize = frame(theme, tick, ramp)
                .iter()
                .map(|line| line.chars().filter(|c| !c.is_whitespace()).count())
                .sum();
            if ink > 0 {
                saw_ink = true;
            }
        }
        assert!(saw_ink, "the blinker never showed anything");
    }
}
