//! Theme-aware color accessors and small color helpers.
//!
//! Extracted from `ui/mod.rs` to keep the giant module focused on the
//! event loop. The function spellings (`c_agent()`, `c_error()`, …)
//! are preserved so call sites read identically to before.

use crossterm::style::Color;

use crate::ui::theme;

// Themed color accessors. These wrap `theme::agent()` etc. so we can
// keep the existing call-site spelling (e.g. `c_agent()` is now a fn).
// Active palette is set at startup via `theme::init`.
#[inline]
pub(crate) fn c_agent() -> Color {
    theme::agent()
}
#[inline]
pub(crate) fn c_error() -> Color {
    theme::error()
}
#[inline]
pub(crate) fn c_tool() -> Color {
    theme::tool()
}
#[inline]
pub(crate) fn c_perm() -> Color {
    theme::perm()
}

/// Map a plugin-supplied color string ("cyan", "red", ...) to a
/// crossterm `Color`. Falls back to dim grey for anything unrecognized
/// so a typo in plugin code doesn't crash the UI.
#[cfg(feature = "plugin")]
pub(crate) fn parse_plugin_color(name: &str) -> Color {
    // Lowercase + strip a leading `:` so `:cyan`, `cyan`, `Cyan` all
    // map to the same crossterm color.
    let normalized = name.trim_start_matches(':').to_ascii_lowercase();
    match normalized.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "darkgrey" | "darkgray" | "grey" | "gray" => Color::DarkGrey,
        "darkred" => Color::DarkRed,
        "darkgreen" => Color::DarkGreen,
        "darkyellow" => Color::DarkYellow,
        "darkblue" => Color::DarkBlue,
        "darkmagenta" => Color::DarkMagenta,
        "darkcyan" => Color::DarkCyan,
        _ => Color::DarkGrey,
    }
}

#[inline]
pub(crate) fn resolve_color(color: Color, monochrome: bool) -> Color {
    if monochrome {
        let _ = color;
        Color::Reset
    } else {
        color
    }
}
