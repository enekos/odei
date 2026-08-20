//! The activity cloud: while the model thinks, a drifting mist breathes in
//! a small block below the `⏺ Thinking…` line. It borrows the splash's
//! two-octave value noise and mist ramp, but never settles — it is weather,
//! not a wordmark, so the drift loops forever until the first token lands.
//!
//! Rendering happens on a spawned thread so the agent loop is never blocked
//! by `sleep`. The sink owns the cloud: it clears the block before printing
//! anything else, and the `Drop` makes sure a forgotten clear can never
//! leave weather frozen on the screen. `NO_COLOR`, a piped stdout, or a
//! non-interactive run means no cloud at all.

use crate::splash;
use crate::theme::{self, Theme};
use std::io::Write as _;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// The mist glyphs, thin to solid — the same ramp the splash condenses from.
const MIST: [char; 7] = [' ', '·', ':', '░', '▒', '▓', '█'];

/// Frames per second. The cloud should breathe, not strobe.
const FRAME: Duration = Duration::from_millis(80);
/// Height of the cloud block, in terminal rows.
pub const ROWS: usize = 3;
/// The drift phase wraps over this many frames, so the loop is seamless.
const CYCLE_FRAMES: u64 = 240;

pub struct Cloud {
    stop: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Cloud {
    /// Reserve the block on screen and start the weather. `Theme` is a
    /// static, so handing it to the render thread costs nothing.
    pub fn start(theme: &'static Theme) -> Cloud {
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
        Cloud { stop: Some(tx), join: Some(join) }
    }

    /// Start a cloud only where one can draw. Non-interactive runs and
    /// unstyled themes get `None`.
    pub fn maybe_start(theme: &'static Theme, interactive: bool) -> Option<Cloud> {
        (interactive && theme.enabled).then(|| Cloud::start(theme))
    }

    /// Erase the block and stop the thread. After this the cursor sits on
    /// the row where the block began, ready for ordinary output. Idempotent.
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

impl Drop for Cloud {
    fn drop(&mut self) {
        self.clear();
    }
}

fn run(theme: &'static Theme, stop: mpsc::Receiver<()>) {
    let mut tick: u64 = 0;
    loop {
        match stop.recv_timeout(FRAME) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let mut out = std::io::stdout().lock();
        for line in frame(theme, tick) {
            let _ = writeln!(out, "{line}\x1b[K");
        }
        // Back to the top of the block, ready for the next frame.
        let _ = write!(out, "\x1b[{ROWS}A");
        let _ = out.flush();
        tick += 1;
    }
}

/// One frame of weather: `ROWS` strings, one per row.
fn frame(theme: &Theme, tick: u64) -> Vec<String> {
    let ramp = theme::mist_ramp(theme);
    let phase = (tick % CYCLE_FRAMES) as f32 / CYCLE_FRAMES as f32 * std::f32::consts::TAU;
    (0..ROWS)
        .map(|row| {
            let mut line = String::new();
            let mut current = usize::MAX;
            for col in 0..cols() {
                // Each row samples the cloud at a slightly different height
                // and drift, so it has depth rather than sliding as a sheet.
                let x = col as f32 * 0.16 + phase * 3.0 + row as f32 * 0.7;
                let y = row as f32 * 0.9 + phase * 0.6;
                let level = level(splash::fbm(x, y, 0x0DE1));
                if level != current {
                    line.push_str(ramp[level]);
                    current = level;
                }
                line.push(MIST[level]);
            }
            format!("{}{}", line.trim_end(), theme.reset())
        })
        .collect()
}

/// Cloud width follows the window, capped so a maximized terminal does not
/// turn a spinner into a landscape.
fn cols() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| (w as usize).saturating_sub(1).min(64))
        .unwrap_or(40)
}

fn level(density: f32) -> usize {
    let steps = (MIST.len() - 1) as f32;
    (density.clamp(0.0, 1.0) * steps).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_the_right_height_and_plain_theme_has_no_escapes() {
        let theme = crate::theme::plain();
        for tick in 0..10 {
            let lines = frame(theme, tick);
            assert_eq!(lines.len(), ROWS);
            for line in lines {
                assert!(!line.contains('\x1b'), "{line:?}");
            }
        }
    }

    #[test]
    fn the_cloud_moves() {
        let theme = crate::theme::plain();
        assert_ne!(frame(theme, 0), frame(theme, 1), "the cloud should drift");
    }

    #[test]
    fn the_cycle_wraps_seamlessly() {
        let theme = crate::theme::plain();
        assert_eq!(
            frame(theme, 0),
            frame(theme, CYCLE_FRAMES),
            "the loop should land back on the first frame"
        );
    }

    #[test]
    fn the_cloud_is_never_all_air() {
        // A spinner that vanishes for a frame reads as a flicker, not weather.
        let theme = crate::theme::plain();
        for tick in 0..CYCLE_FRAMES {
            let ink: usize = frame(theme, tick)
                .iter()
                .map(|line| line.chars().filter(|c| *c != ' ').count())
                .sum();
            assert!(ink > 0, "frame {tick} vanished");
        }
    }
}
