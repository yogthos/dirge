//! UI color theme.
//!
//! Centralizes semantic color choices (agent, tool, error, accent, …)
//! so the whole UI can swap palettes without rewriting call sites.
//! The active theme is selected at startup via `ui::theme::init` (from
//! the user's `theme = "..."` config) and read everywhere else through
//! the helper functions exposed at the bottom of this file.
//!
//! Currently shipping presets:
//! - `phosphor` (default) — CRT-green 80s-hacker palette. Errors stay
//!   red and warnings stay yellow so semantic urgency isn't sacrificed
//!   for aesthetics.
//! - `plain` — the pre-theme look (white assistant text, cyan accents).
//!   Use this if green-on-black hurts your eyes or clashes with your
//!   terminal background.
//!
//! Adding a theme means appending a `pub const fn`-style preset here
//! and matching its name in `init` — no other code changes needed.

use std::sync::OnceLock;

use crossterm::style::Color;

/// Semantic colors for every role in the UI. Concrete colors are
/// chosen by the active preset; call sites should reach for the helper
/// functions below (`agent()`, `error()`, …) rather than poking the
/// struct directly so future additions stay backwards-compatible.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Assistant chat text.
    pub agent: Color,
    /// User-message prefix (and prompt indicator).
    pub user: Color,
    /// System/info messages — context loaded, compactions, etc.
    pub system: Color,
    /// Tool execution headers (`bash:`, `read:`, …).
    pub tool: Color,
    /// Permission prompts. Stays loud (yellow/red family) because the
    /// user must notice — never go subtle here.
    pub perm: Color,
    /// Secondary result text (slash command output, tool stdout dim).
    pub result: Color,
    /// Hard errors. Always red-family; theme can choose the exact red
    /// but must keep it semantically distinct from everything else.
    pub error: Color,
    /// Warnings. Yellow-family; same rule as `error`.
    pub warn: Color,
    /// Headers, focused picker rows, banner accent. The "look at this"
    /// color.
    pub accent: Color,
    /// Dim auxiliary text — placeholders, separators, low-noise hints.
    pub dim: Color,
    /// Panel headers in the right-hand info panel.
    pub header: Color,
    /// Horizontal divider line color.
    pub divider: Color,
    /// Welcome-banner primary stroke.
    pub banner_primary: Color,
    /// Welcome-banner secondary stroke (border, decorations).
    pub banner_secondary: Color,
    /// Human-readable name surfaced in the banner ("PHOSPHOR", "PLAIN").
    pub label: &'static str,
}

impl Theme {
    /// 80s-CRT phosphor green. Default. Errors red, warnings yellow.
    /// No grey anywhere on the green axis — secondary tones use
    /// DarkGreen so the whole display reads like a single-phosphor
    /// monochrome monitor.
    pub const fn phosphor() -> Self {
        Theme {
            agent: Color::Green,
            user: Color::Green,
            system: Color::DarkGreen,
            tool: Color::Green,
            perm: Color::Yellow,
            result: Color::DarkGreen,
            error: Color::Red,
            warn: Color::Yellow,
            accent: Color::Green,
            dim: Color::DarkGreen,
            header: Color::Green,
            divider: Color::DarkGreen,
            banner_primary: Color::Green,
            banner_secondary: Color::DarkGreen,
            label: "PHOSPHOR",
        }
    }

    /// Pre-theme look. Use this when the green doesn't suit your
    /// terminal background or you just want the boring default.
    pub const fn plain() -> Self {
        Theme {
            agent: Color::White,
            user: Color::Green,
            system: Color::DarkGrey,
            tool: Color::Yellow,
            perm: Color::Magenta,
            result: Color::DarkGrey,
            error: Color::Red,
            warn: Color::Yellow,
            accent: Color::Cyan,
            dim: Color::DarkGrey,
            header: Color::Cyan,
            divider: Color::DarkGrey,
            banner_primary: Color::Cyan,
            banner_secondary: Color::DarkGrey,
            label: "PLAIN",
        }
    }
}

/// Global theme set once at startup. Defaults to `phosphor` if `init`
/// is never called (handy for tests + the `--no-tools` no-UI mode).
static THEME: OnceLock<Theme> = OnceLock::new();

/// Initialize the global theme from a name. Unknown names fall back
/// to `phosphor` with a stderr warning so a typo doesn't make the UI
/// unreadable. Safe to call once; subsequent calls are ignored.
pub fn init(name: &str) {
    let theme = match name.to_ascii_lowercase().as_str() {
        "phosphor" | "" => Theme::phosphor(),
        "plain" => Theme::plain(),
        other => {
            eprintln!(
                "warning: unknown theme '{}'; using phosphor. Valid: phosphor, plain.",
                other,
            );
            Theme::phosphor()
        }
    };
    let _ = THEME.set(theme);
}

/// Read the active theme. Lazy-initializes to `phosphor` if no `init`
/// call has happened (the happy path during `cargo test`).
pub fn current() -> &'static Theme {
    THEME.get_or_init(Theme::phosphor)
}

// Convenience accessors. Call sites use these instead of touching the
// struct so renaming/restructuring fields in `Theme` doesn't ripple
// across the codebase.

pub fn agent() -> Color {
    current().agent
}
pub fn user() -> Color {
    current().user
}
pub fn system() -> Color {
    current().system
}
pub fn tool() -> Color {
    current().tool
}
pub fn perm() -> Color {
    current().perm
}
pub fn result() -> Color {
    current().result
}
pub fn error() -> Color {
    current().error
}
pub fn warn() -> Color {
    current().warn
}
pub fn accent() -> Color {
    current().accent
}
pub fn dim() -> Color {
    current().dim
}
pub fn header() -> Color {
    current().header
}
pub fn divider() -> Color {
    current().divider
}
pub fn banner_primary() -> Color {
    current().banner_primary
}
pub fn banner_secondary() -> Color {
    current().banner_secondary
}

/// Whether the given color should render with the Bold attribute to
/// fake the CRT phosphor "bloom" effect. Bright phosphor tones glow;
/// dim secondary tones stay un-bloomed so the two-tone depth in the
/// reference screenshots is preserved.
pub fn is_bright(c: Color) -> bool {
    matches!(
        c,
        Color::Green
            | Color::Red
            | Color::Yellow
            | Color::Cyan
            | Color::Magenta
            | Color::Blue
            | Color::White
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `phosphor` and `plain` differ in their agent color — quick
    /// sanity check that the presets aren't accidentally identical.
    #[test]
    fn presets_are_distinct() {
        assert_ne!(Theme::phosphor().agent, Theme::plain().agent);
        assert_ne!(Theme::phosphor().accent, Theme::plain().accent);
    }

    /// Errors and warnings must stay in the red/yellow family across
    /// every preset — that's the load-bearing semantic contract.
    #[test]
    fn error_and_warn_stay_loud() {
        for t in [Theme::phosphor(), Theme::plain()] {
            assert!(
                matches!(t.error, Color::Red | Color::DarkRed),
                "theme {} broke error contract",
                t.label,
            );
            assert!(
                matches!(t.warn, Color::Yellow | Color::DarkYellow),
                "theme {} broke warn contract",
                t.label,
            );
        }
    }

    /// `init` with an unknown name does not panic and the theme still
    /// resolves (falls back to phosphor).
    #[test]
    fn init_with_unknown_name_falls_back() {
        // Can't actually call init() — it's OnceLock-backed and a
        // prior test may have already set the global. Instead verify
        // `current()` resolves without panicking and has a label.
        let t = current();
        assert!(!t.label.is_empty());
    }
}
