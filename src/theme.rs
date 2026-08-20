//! Visual language: monochrome grayscale, with color reserved for diff
//! markers. Dark theme is the default; light theme swaps to the darker
//! gray ramp.

use std::io::IsTerminal;

// Themes are plain static string tables; sharing one with the cloud's
// render thread is safe.
unsafe impl Sync for Theme {}

pub const RESET: &str = "\x1b[0m";
#[allow(dead_code)]
pub const BOLD: &str = "\x1b[1m";

pub const INPUT_PREFIX: &str = "❯ ";
pub const ASK_ACTIVITY_LABEL: &str = "⏺ Thinking";

// Some styles (divider, diff markers, tag) are defined for completeness
// and used as rendering grows.
#[allow(dead_code)]
pub struct Theme {
    pub enabled: bool,
    pub is_light: bool,
    pub divider: &'static str,
    pub hint: &'static str,
    pub statusline: &'static str,
    pub tag: &'static str,
    pub subtitle: &'static str,
    pub system_notice_label: &'static str,
    pub system_notice_text: &'static str,
    pub dim: &'static str,
    pub warning: &'static str,
    pub approval_button_active: &'static str,
    pub approval_button_inactive: &'static str,
    pub selected_completion: &'static str,
    pub permission_auto: &'static str,
    // The line number and +/- sign carry the only color in an otherwise
    // monochrome diff: green for additions (#30A46C), red for deletions
    // (#E5484D). The line text stays neutral.
    pub diff_added_marker: &'static str,
    pub diff_removed_marker: &'static str,
    // Markdown. Structure is carried by weight, spacing and a background —
    // never by hue, so the grayscale rule survives an answer full of code
    // and tables.
    pub heading: &'static str,
    pub strong: &'static str,
    pub emphasis: &'static str,
    pub strike: &'static str,
    pub link: &'static str,
    /// `inline code`: a panel rather than a colour.
    pub code: &'static str,
    pub code_block: &'static str,
    pub quote: &'static str,
    pub quote_bar: &'static str,
    pub bullet: &'static str,
    pub table_header: &'static str,
}

const DARK: Theme = Theme {
    enabled: true,
    is_light: false,
    divider: "\x1b[38;5;240m",
    hint: "\x1b[38;5;255m",
    statusline: "\x1b[38;5;245m",
    tag: "\x1b[1;38;5;255m",
    subtitle: "\x1b[1;38;5;255m",
    system_notice_label: "\x1b[1;38;5;252m",
    system_notice_text: "\x1b[38;5;250m",
    dim: "\x1b[38;5;245m",
    warning: "\x1b[38;5;252m",
    approval_button_active: "\x1b[48;5;255m\x1b[38;5;235m\x1b[1m",
    approval_button_inactive: "\x1b[48;5;239m\x1b[38;5;255m",
    selected_completion: "\x1b[1;38;5;255m",
    permission_auto: "\x1b[38;5;252m",
    diff_added_marker: "\x1b[38;2;48;164;108m",
    diff_removed_marker: "\x1b[38;2;229;72;77m",
    heading: "\x1b[1;38;5;255m",
    strong: "\x1b[1m",
    emphasis: "\x1b[3m",
    strike: "\x1b[9m",
    link: "\x1b[4m",
    code: "\x1b[48;5;236m\x1b[38;5;253m",
    code_block: "\x1b[38;5;252m",
    quote: "\x1b[3;38;5;245m",
    quote_bar: "\x1b[38;5;240m",
    bullet: "\x1b[38;5;245m",
    table_header: "\x1b[1;38;5;255m",
};

const LIGHT: Theme = Theme {
    enabled: true,
    is_light: true,
    divider: "\x1b[38;5;250m",
    hint: "\x1b[38;5;235m",
    statusline: "\x1b[38;5;241m",
    tag: "\x1b[1;38;5;235m",
    subtitle: "\x1b[1;38;5;235m",
    system_notice_label: "\x1b[1;38;5;238m",
    system_notice_text: "\x1b[38;5;241m",
    dim: "\x1b[38;5;247m",
    warning: "\x1b[38;5;238m",
    approval_button_active: "\x1b[48;5;236m\x1b[38;5;255m\x1b[1m",
    approval_button_inactive: "\x1b[48;5;251m\x1b[38;5;237m",
    selected_completion: "\x1b[1;38;5;235m",
    permission_auto: "\x1b[38;5;238m",
    diff_added_marker: "\x1b[38;2;48;164;108m",
    diff_removed_marker: "\x1b[38;2;229;72;77m",
    heading: "\x1b[1;38;5;235m",
    strong: "\x1b[1m",
    emphasis: "\x1b[3m",
    strike: "\x1b[9m",
    link: "\x1b[4m",
    code: "\x1b[48;5;253m\x1b[38;5;235m",
    code_block: "\x1b[38;5;238m",
    quote: "\x1b[3;38;5;247m",
    quote_bar: "\x1b[38;5;250m",
    bullet: "\x1b[38;5;247m",
    table_header: "\x1b[1;38;5;235m",
};

const PLAIN: Theme = Theme {
    enabled: false,
    is_light: false,
    divider: "",
    hint: "",
    statusline: "",
    tag: "",
    subtitle: "",
    system_notice_label: "",
    system_notice_text: "",
    dim: "",
    warning: "",
    approval_button_active: "",
    approval_button_inactive: "",
    selected_completion: "",
    permission_auto: "",
    diff_added_marker: "",
    diff_removed_marker: "",
    heading: "",
    strong: "",
    emphasis: "",
    strike: "",
    link: "",
    code: "",
    code_block: "",
    quote: "",
    quote_bar: "",
    bullet: "",
    table_header: "",
};

/// The splash mist, thin → solid. The whole point of the grayscale rule is
/// that a ramp like this can carry depth on its own, so the cloud is eight
/// steps of gray and no hue at all.
pub const MIST_DARK: [&str; 7] = [
    "",
    "\x1b[38;5;238m",
    "\x1b[38;5;241m",
    "\x1b[38;5;245m",
    "\x1b[38;5;248m",
    "\x1b[38;5;252m",
    "\x1b[38;5;255m",
];

const MIST_LIGHT: [&str; 7] = [
    "",
    "\x1b[38;5;251m",
    "\x1b[38;5;249m",
    "\x1b[38;5;246m",
    "\x1b[38;5;242m",
    "\x1b[38;5;238m",
    "\x1b[38;5;235m",
];

const MIST_PLAIN: [&str; 7] = ["", "", "", "", "", "", ""];

pub fn mist_ramp(theme: &Theme) -> &'static [&'static str; 7] {
    if !theme.enabled {
        &MIST_PLAIN
    } else if theme.is_light {
        &MIST_LIGHT
    } else {
        &MIST_DARK
    }
}

/// The unstyled theme, also used to measure markup-free text.
pub fn plain() -> &'static Theme {
    &PLAIN
}

#[cfg(test)]
pub fn dark() -> &'static Theme {
    &DARK
}

impl Theme {
    pub fn detect() -> &'static Theme {
        if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
            return &PLAIN;
        }
        match std::env::var("ODEI_THEME").as_deref() {
            Ok("light") => &LIGHT,
            Ok("dark") => &DARK,
            _ => {
                // COLORFGBG "15;0"-style hint: last field is the background.
                if let Ok(v) = std::env::var("COLORFGBG") {
                    if let Some(bg) = v.rsplit(';').next() {
                        if matches!(bg, "7" | "15") {
                            return &LIGHT;
                        }
                    }
                }
                &DARK
            }
        }
    }

    pub fn reset(&self) -> &'static str {
        if self.enabled {
            RESET
        } else {
            ""
        }
    }

    #[allow(dead_code)]
    pub fn bold(&self) -> &'static str {
        if self.enabled {
            BOLD
        } else {
            ""
        }
    }
}

/// The one-line greeting, used wherever the splash cannot draw: a pipe, a
/// window too narrow for the wordmark, `NO_COLOR`, `ODEI_SPLASH=off`.
pub fn welcome_message(theme: &Theme, version: &str) -> String {
    format!(
        "{}𝒐dei{}{} v{} · Run /help for commands{}\n",
        theme.subtitle,
        theme.reset(),
        theme.dim,
        version,
        theme.reset()
    )
}
