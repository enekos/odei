//! Startup splash. `odei` is Basque for *cloud*, so the wordmark condenses
//! out of drifting mist: a two-octave value-noise cloud blows across the
//! field while the glyph cells thicken to solid and everything else thins to
//! nothing. Grayscale only — the theme keeps colour for diff markers. The
//! cloud is seeded from the clock, so no two launches condense the same way.
//!
//! Everything degrades. No terminal, `NO_COLOR`, a window too narrow for the
//! wordmark or `ODEI_SPLASH=off` prints the one-line welcome instead;
//! `ODEI_SPLASH=static`, or a window too short to redraw in place, draws the
//! settled frame once without the animation. Ctrl+C during the animation
//! cuts straight to the settled frame rather than leaving half a cloud.

use crate::theme::{self, Theme};
use std::io::Write;
use std::sync::atomic::Ordering;

/// The wordmark as a pixel canvas: `#` is ink, everything else is air. A
/// pixel is drawn two cells wide, which makes it square on a terminal grid.
const WORDMARK: [&str; 7] = [
    "          #        ",
    "          #       #",
    " ###   ####  ###   ",
    "#   # #   # #   # #",
    "#   # #   # ##### #",
    "#   # #   # #     #",
    " ###   ####  ###  #",
];
const WORD_COLS: usize = 19;
const WORD_ROWS: usize = WORDMARK.len();

const CELL: usize = 2;
/// Grid columns of clear air left of the wordmark — also the text indent for
/// the lines underneath — and of mist blowing off to the right.
const PAD_LEFT: usize = 2;
const PAD_RIGHT: usize = 8;
/// A row of sky above and below, so the cloud has somewhere to be.
const SKY: usize = 1;
/// Lines of chrome under the field: horizon, two dim lines, the blank above.
/// Without that much room the cursor-up redraw fights the terminal's scroll.
const CHROME: usize = 4;

const FRAMES: usize = 22;
const FRAME_MS: u64 = 20;

/// Thin to solid. Two cells of the same character make one pixel.
const MIST: [char; 7] = [' ', '·', ':', '░', '▒', '▓', '█'];

/// The condensing field: a mist canvas with the wordmark stencilled into it.
struct Field {
    cols: usize,
    rows: usize,
    seed: u64,
}

impl Field {
    /// Size the field to the window, or `None` if the wordmark will not fit.
    fn fit(width: usize) -> Option<Field> {
        let budget = width.saturating_sub(1) / CELL;
        if budget < PAD_LEFT + WORD_COLS {
            return None;
        }
        Some(Field {
            cols: budget.min(PAD_LEFT + WORD_COLS + PAD_RIGHT),
            rows: WORD_ROWS + SKY * 2,
            seed: seed(),
        })
    }

    fn glyph_on(&self, col: usize, row: usize) -> bool {
        let (Some(x), Some(y)) = (col.checked_sub(PAD_LEFT), row.checked_sub(SKY)) else {
            return false;
        };
        WORDMARK
            .get(y)
            .and_then(|line| line.as_bytes().get(x))
            .is_some_and(|c| *c == b'#')
    }

    /// How thick this pixel is at `progress` (0 = all cloud, 1 = settled).
    fn density(&self, col: usize, row: usize, progress: f32) -> f32 {
        // The cloud drifts as the frames advance; sampling further along it
        // is what makes the mist move instead of merely fade.
        let wind = progress * 14.0;
        let cloud = fbm(col as f32 * 0.30 + wind, row as f32 * 0.55, self.seed);
        if self.glyph_on(col, row) {
            // Ink starts as thick cloud and ends solid, late and quickly, so
            // the letters look like they resolve rather than like they wipe.
            let solid = ease(progress).powf(1.4);
            (0.34 + cloud * 0.5) * (1.0 - solid) + solid
        } else {
            // Mist holds most of its body and then goes fast. A plain
            // `1 - progress` fade quantizes to nothing two thirds of the way
            // in, which wastes the last frames on an already-still wordmark.
            cloud * self.vignette(col, row) * (1.0 - progress.powf(2.2)) * 0.9
        }
    }

    /// Soft edges: the cloud thins toward the sides and into the sky rows, so
    /// the field reads as weather instead of as a rectangle of noise.
    fn vignette(&self, col: usize, row: usize) -> f32 {
        let from_side = col.min(self.cols.saturating_sub(1) - col);
        let side = ((from_side as f32 + 0.5) / 4.0).min(1.0);
        let sky = if (SKY..SKY + WORD_ROWS).contains(&row) {
            1.0
        } else {
            0.5
        };
        side * sky
    }

    /// One frame, one string per row, styled for `theme`.
    fn frame(&self, theme: &Theme, progress: f32) -> Vec<String> {
        let ramp = theme::mist_ramp(theme);
        (0..self.rows)
            .map(|row| {
                let mut line = String::new();
                let mut current = usize::MAX;
                for col in 0..self.cols {
                    let level = level(self.density(col, row, progress));
                    if level != current {
                        line.push_str(ramp[level]);
                        current = level;
                    }
                    for _ in 0..CELL {
                        line.push(MIST[level]);
                    }
                }
                // Air at the end of a row is just whitespace; the redraw
                // clears to end of line anyway.
                format!("{}{}", line.trim_end(), theme.reset())
            })
            .collect()
    }

    /// The ground the cloud settles on: a rule under the wordmark that fades
    /// out to the right.
    fn horizon(&self, theme: &Theme) -> String {
        let ramp = theme::mist_ramp(theme);
        let width = WORD_COLS * CELL;
        let mut line = " ".repeat(PAD_LEFT * CELL);
        let mut current = usize::MAX;
        for i in 0..width {
            let fade = 1.0 - i as f32 / width as f32;
            let level = (fade * 3.0).round() as usize + 1;
            if level != current {
                line.push_str(ramp[level]);
                current = level;
            }
            line.push('▁');
        }
        format!("{}{}", line, theme.reset())
    }
}

enum Mode {
    /// One line of text — piped output, `NO_COLOR`, or opted out.
    Line,
    /// The settled frame, drawn once.
    Still,
    /// The full condense.
    Condense,
}

fn mode(theme: &Theme) -> Mode {
    match std::env::var("ODEI_SPLASH").as_deref() {
        Ok("off") | Ok("0") | Ok("no") | Ok("none") | Ok("line") => Mode::Line,
        Ok("static") | Ok("still") => Mode::Still,
        // A non-terminal stdout is already what turns the theme off.
        _ if !theme.enabled => Mode::Line,
        _ => Mode::Condense,
    }
}

/// Print the splash. `subtitle` is the dim line under the wordmark, after the
/// version.
pub fn show(theme: &Theme, version: &str, subtitle: &str) {
    let (width, height) = crossterm::terminal::size().unwrap_or((0, 0));
    let mode = mode(theme);
    let field = match mode {
        Mode::Line => None,
        _ => Field::fit(width as usize),
    };
    let Some(field) = field else {
        print!("{}", theme::welcome_message(theme, version));
        return;
    };
    let condense = matches!(mode, Mode::Condense) && height as usize >= field.rows + CHROME;

    let mut out = std::io::stdout().lock();
    let _ = writeln!(out);
    // The cursor would sit inside the cloud, blinking.
    if condense {
        let _ = write!(out, "\x1b[?25l");
    }
    let mut drawn = false;
    if condense {
        // Stops short of 1.0: the settled frame below is the last one, and
        // drawing something indistinguishable from it first only stalls.
        for step in 0..FRAMES - 1 {
            draw(&mut out, &field, theme, step as f32 / FRAMES as f32, drawn);
            drawn = true;
            if crate::ui::CANCEL.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(FRAME_MS));
        }
    }
    // The settled frame is always the last one on screen, interrupted or not.
    draw(&mut out, &field, theme, 1.0, drawn);
    if condense {
        let _ = write!(out, "\x1b[?25h");
    }

    let indent = " ".repeat(PAD_LEFT * CELL);
    let _ = writeln!(out, "{}", field.horizon(theme));
    let _ = writeln!(
        out,
        "{indent}{}v{version} · {subtitle}{}",
        theme.dim,
        theme.reset()
    );
    let _ = writeln!(
        out,
        "{indent}{}/help for commands{}",
        theme.dim,
        theme.reset()
    );
    let _ = out.flush();
}

fn draw(out: &mut impl Write, field: &Field, theme: &Theme, progress: f32, redraw: bool) {
    if redraw {
        let _ = write!(out, "\x1b[{}A", field.rows);
    }
    for line in field.frame(theme, progress) {
        // Clear to end of line: the previous frame may have reached further.
        let _ = writeln!(out, "{line}\x1b[K");
    }
    let _ = out.flush();
}

fn level(density: f32) -> usize {
    let steps = (MIST.len() - 1) as f32;
    (density.clamp(0.0, 1.0) * steps).round() as usize
}

fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Two octaves of value noise — one for the shape of the cloud, one for the
/// grain in it.
pub(crate) fn fbm(x: f32, y: f32, seed: u64) -> f32 {
    let coarse = noise(x, y, seed);
    let fine = noise(x * 2.1 + 5.0, y * 2.1 + 11.0, seed ^ 0x5DEE_CE66);
    (coarse * 0.66 + fine * 0.34).clamp(0.0, 1.0)
}

/// Value noise: a random value per lattice point, smoothly interpolated.
/// Hashed rather than stored, so any point of the cloud can be sampled at any
/// drift offset without keeping a grid around.
fn noise(x: f32, y: f32, seed: u64) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (ease(x - xi), ease(y - yi));
    let (xi, yi) = (xi as i64, yi as i64);
    let top = lerp(hash(xi, yi, seed), hash(xi + 1, yi, seed), fx);
    let bottom = lerp(hash(xi, yi + 1, seed), hash(xi + 1, yi + 1, seed), fx);
    lerp(top, bottom, fy)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn hash(x: i64, y: i64, seed: u64) -> f32 {
    let mut h = seed
        ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 29;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 32;
    (h >> 40) as f32 / (1u64 << 24) as f32
}

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0x0DE1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(seed: u64) -> Field {
        Field {
            cols: PAD_LEFT + WORD_COLS + PAD_RIGHT,
            rows: WORD_ROWS + SKY * 2,
            seed,
        }
    }

    #[test]
    fn wordmark_canvas_is_rectangular() {
        for line in WORDMARK {
            assert_eq!(line.chars().count(), WORD_COLS, "{line:?}");
            assert!(line.chars().all(|c| c == '#' || c == ' '), "{line:?}");
        }
    }

    #[test]
    fn settled_frame_is_the_wordmark_and_nothing_else() {
        let field = field(1);
        let lines = field.frame(theme::plain(), 1.0);
        assert_eq!(lines.len(), WORD_ROWS + SKY * 2);
        // Sky rows are empty, glyph rows are the canvas at two cells a pixel.
        assert_eq!(lines[0], "");
        assert_eq!(lines[lines.len() - 1], "");
        for (row, canvas) in WORDMARK.iter().enumerate() {
            let ink = canvas.chars().filter(|c| *c == '#').count();
            let drawn = lines[row + SKY].chars().filter(|c| *c == '█').count();
            assert_eq!(drawn, ink * CELL, "row {row}: {:?}", lines[row + SKY]);
        }
        // Nothing but ink survives the condense.
        let mist: String = lines.join("");
        assert!(!mist.contains('░') && !mist.contains('▒') && !mist.contains('·'));
    }

    #[test]
    fn plain_theme_emits_no_escapes() {
        let field = field(7);
        for progress in [0.0, 0.5, 1.0] {
            for line in field.frame(theme::plain(), progress) {
                assert!(!line.contains('\x1b'), "{line:?}");
            }
        }
        assert!(!field.horizon(theme::plain()).contains('\x1b'));
    }

    #[test]
    fn styled_theme_only_paints_grays() {
        let field = field(7);
        let painted = field.frame(theme::dark(), 0.4).join("");
        assert!(painted.contains("\x1b[38;5;"));
        // 38;2 is truecolor, which in this theme means a diff marker.
        assert!(!painted.contains("\x1b[38;2;"));
    }

    #[test]
    fn mist_fills_the_field_then_blows_off() {
        let field = field(42);
        let outside = |progress: f32| {
            (0..field.rows)
                .flat_map(|row| (0..field.cols).map(move |col| (col, row)))
                .filter(|(col, row)| !field.glyph_on(*col, *row))
                .filter(|(col, row)| level(field.density(*col, *row, progress)) > 0)
                .count()
        };
        assert!(outside(0.0) > 20, "opening frame should be cloudy");
        assert!(outside(0.5) < outside(0.0), "mist should be thinning");
        assert_eq!(outside(1.0), 0, "settled frame should be clear");
    }

    #[test]
    fn glyphs_are_never_blank_even_in_the_first_frame() {
        // A letter cell that reads as air would make the wordmark flicker.
        let field = field(3);
        for row in 0..field.rows {
            for col in 0..field.cols {
                if field.glyph_on(col, row) {
                    assert!(level(field.density(col, row, 0.0)) > 0, "({col},{row})");
                }
            }
        }
    }

    #[test]
    fn the_cloud_depends_on_the_seed() {
        let a = field(11).frame(theme::plain(), 0.25);
        assert_eq!(a, field(11).frame(theme::plain(), 0.25));
        assert_ne!(a, field(12).frame(theme::plain(), 0.25));
    }

    #[test]
    fn narrow_windows_get_no_field() {
        assert!(Field::fit(0).is_none());
        assert!(Field::fit(40).is_none());
        let wide = Field::fit(200).expect("field fits");
        // Capped, so the cloud does not smear across a maximized window.
        assert_eq!(wide.cols, PAD_LEFT + WORD_COLS + PAD_RIGHT);
        // Exactly wide enough for the wordmark, and no mist to spare.
        let snug = Field::fit((PAD_LEFT + WORD_COLS) * CELL + 1).expect("field fits");
        assert_eq!(snug.cols, PAD_LEFT + WORD_COLS);
    }

    #[test]
    fn noise_stays_in_range() {
        for i in 0..200 {
            let (x, y) = (i as f32 * 0.37, i as f32 * 0.11);
            let n = fbm(x, y, 0xABCD);
            assert!((0.0..=1.0).contains(&n), "{n}");
        }
    }
}
