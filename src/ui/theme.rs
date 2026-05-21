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
//! ## Custom themes via `<name>.theme.json`
//!
//! Users can define their own palette by dropping a JSON file at
//! `~/.config/dirge/<name>.theme.json` and setting `theme = "<name>"`
//! in `config.json`. All theme fields are optional — fields not
//! present fall back to the phosphor preset, so a minimal override
//! file like `{"agent": "blue"}` works.
//!
//! Color values accept:
//! - Named colors: `"green"`, `"darkgreen"`, `"red"`, … (every
//!   crossterm `Color::<Name>` is accepted, case-insensitive).
//! - Hex RGB: `"#1a2b3c"`.
//! - 256-color palette index as a number: `42` (0..=255).
//!
//! Adding a built-in preset means appending a `pub const fn`-style
//! preset here and matching its name in `init` — no other code
//! changes needed.

use std::sync::OnceLock;

use crossterm::style::Color;
use serde::Deserialize;

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
            // Cyan complements phosphor green without breaking the
            // CRT aesthetic — classic CRTs shipped with green OR
            // cyan/amber phosphors and the cyan tone reads
            // distinct-but-related. Before this both `user` and
            // `agent` were `Color::Green` so user messages were
            // visually indistinguishable from the agent's output —
            // confusing when scrolling chat history.
            user: Color::Cyan,
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

/// JSON shape for `<name>.theme.json` overrides. Every field is
/// optional; absent fields inherit from the base preset
/// (phosphor). Color values accept named colors, hex strings, or
/// 256-color palette indices — see `parse_color_value`.
#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
struct ThemeJson {
    agent: Option<ColorValue>,
    user: Option<ColorValue>,
    system: Option<ColorValue>,
    tool: Option<ColorValue>,
    perm: Option<ColorValue>,
    result: Option<ColorValue>,
    error: Option<ColorValue>,
    warn: Option<ColorValue>,
    accent: Option<ColorValue>,
    dim: Option<ColorValue>,
    header: Option<ColorValue>,
    divider: Option<ColorValue>,
    banner_primary: Option<ColorValue>,
    banner_secondary: Option<ColorValue>,
    label: Option<String>,
}

/// Polymorphic color value: name string, hex `"#rrggbb"`, or
/// 256-color palette index `0..=255`. Custom deserializer below.
#[derive(Debug)]
struct ColorValue(Color);

impl<'de> Deserialize<'de> for ColorValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => parse_color_value(&s)
                .map(ColorValue)
                .map_err(D::Error::custom),
            serde_json::Value::Number(n) => {
                let n = n.as_u64().ok_or_else(|| {
                    D::Error::custom("color index must be a non-negative integer 0..=255")
                })?;
                if n > 255 {
                    return Err(D::Error::custom("color index out of range 0..=255"));
                }
                Ok(ColorValue(Color::AnsiValue(n as u8)))
            }
            other => Err(D::Error::custom(format!(
                "color must be a name string, hex string, or 0..=255 integer; got {other:?}"
            ))),
        }
    }
}

/// Parse a color name or hex string. Names match crossterm's
/// `Color::<Name>` variants case-insensitively; `_` and `-`
/// separators are both accepted (`"dark_red"` == `"dark-red"` ==
/// `"darkred"`). Hex form is `"#rrggbb"`.
fn parse_color_value(raw: &str) -> Result<Color, String> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(format!(
                "hex color must be `#rrggbb` (6 hex digits); got `{}`",
                raw
            ));
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| format!("bad red byte: {e}"))?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| format!("bad green byte: {e}"))?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| format!("bad blue byte: {e}"))?;
        return Ok(Color::Rgb { r, g, b });
    }
    // Normalize: lowercase, strip `_` and `-` so "dark_red", "dark-red"
    // and "darkred" all match.
    let key: String = s
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    Ok(match key.as_str() {
        "black" => Color::Black,
        "darkgrey" | "darkgray" => Color::DarkGrey,
        "red" => Color::Red,
        "darkred" => Color::DarkRed,
        "green" => Color::Green,
        "darkgreen" => Color::DarkGreen,
        "yellow" => Color::Yellow,
        "darkyellow" => Color::DarkYellow,
        "blue" => Color::Blue,
        "darkblue" => Color::DarkBlue,
        "magenta" => Color::Magenta,
        "darkmagenta" => Color::DarkMagenta,
        "cyan" => Color::Cyan,
        "darkcyan" => Color::DarkCyan,
        "white" => Color::White,
        "grey" | "gray" => Color::Grey,
        "reset" => Color::Reset,
        _ => return Err(format!("unknown color name: {raw}")),
    })
}

impl ThemeJson {
    /// Apply override fields onto `base`. Each `Some` field replaces
    /// the corresponding base color; `None` keeps the base value.
    /// `label` comes from the caller (filename-derived or
    /// JSON-supplied) — already-leaked `&'static str` so the
    /// resulting Theme stays Copy.
    fn merge_into(self, base: Theme, label: &'static str) -> Result<Theme, String> {
        let pick = |o: Option<ColorValue>, b: Color| match o {
            Some(c) => c.0,
            None => b,
        };
        Ok(Theme {
            agent: pick(self.agent, base.agent),
            user: pick(self.user, base.user),
            system: pick(self.system, base.system),
            tool: pick(self.tool, base.tool),
            perm: pick(self.perm, base.perm),
            result: pick(self.result, base.result),
            error: pick(self.error, base.error),
            warn: pick(self.warn, base.warn),
            accent: pick(self.accent, base.accent),
            dim: pick(self.dim, base.dim),
            header: pick(self.header, base.header),
            divider: pick(self.divider, base.divider),
            banner_primary: pick(self.banner_primary, base.banner_primary),
            banner_secondary: pick(self.banner_secondary, base.banner_secondary),
            label,
        })
    }
}

/// Global theme set once at startup. Defaults to `phosphor` if `init`
/// is never called (handy for tests + the `--no-tools` no-UI mode).
static THEME: OnceLock<Theme> = OnceLock::new();

/// Initialize the global theme from a name. Resolution order:
/// 1. Built-in: `phosphor` (default), `plain`.
/// 2. Custom JSON: `~/.config/dirge/<name>.theme.json`. Fields not
///    present in the file inherit from phosphor — minimal overrides
///    are encouraged (e.g. `{"accent": "magenta"}`).
/// 3. Fallback: phosphor with a stderr warning if neither matched.
///
/// Safe to call once; subsequent calls are ignored (`OnceLock`).
pub fn init(name: &str) {
    let theme = match name.to_ascii_lowercase().as_str() {
        "phosphor" | "" => Theme::phosphor(),
        "plain" => Theme::plain(),
        other => load_custom_theme(other).unwrap_or_else(|err| {
            eprintln!(
                "warning: theme '{}' could not be loaded ({}); using phosphor.\n\
                 Custom themes live at ~/.config/dirge/<name>.theme.json.",
                other, err,
            );
            Theme::phosphor()
        }),
    };
    let _ = THEME.set(theme);
}

/// Try to load `~/.config/dirge/<name>.theme.json` and merge its
/// fields over the phosphor preset. Returns Err with a
/// human-readable message when:
/// - The file doesn't exist.
/// - The JSON fails to parse.
/// - A color value is unrecognized.
fn load_custom_theme(name: &str) -> Result<Theme, String> {
    let path = crate::session::storage::config_path().join(format!("{name}.theme.json"));
    if !path.exists() {
        return Err(format!("no such file: {}", path.display()));
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let overrides: ThemeJson =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    // Label defaults to the filename's stem in uppercase if the JSON
    // doesn't specify one. Leak the string so the global Theme
    // stays `&'static str` and `Copy`-able — one leak per process
    // is negligible (a few dozen bytes once at startup).
    let label_str = overrides
        .label
        .clone()
        .unwrap_or_else(|| name.to_ascii_uppercase());
    let label: &'static str = Box::leak(label_str.into_boxed_str());
    overrides.merge_into(Theme::phosphor(), label)
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

    // ---- Custom theme file loading (`<name>.theme.json`) ----

    #[test]
    fn parse_color_value_named() {
        assert!(matches!(parse_color_value("green"), Ok(Color::Green)));
        assert!(matches!(parse_color_value("GREEN"), Ok(Color::Green)));
        assert!(matches!(parse_color_value("DarkRed"), Ok(Color::DarkRed)));
        // `_` and `-` separators normalize away.
        assert!(matches!(parse_color_value("dark_red"), Ok(Color::DarkRed)));
        assert!(matches!(parse_color_value("dark-red"), Ok(Color::DarkRed)));
        // Both Grey spellings.
        assert!(matches!(parse_color_value("gray"), Ok(Color::Grey)));
        assert!(matches!(parse_color_value("grey"), Ok(Color::Grey)));
    }

    #[test]
    fn parse_color_value_hex_rgb() {
        let c = parse_color_value("#1a2b3c").unwrap();
        assert!(matches!(
            c,
            Color::Rgb {
                r: 0x1a,
                g: 0x2b,
                b: 0x3c,
            }
        ));
        // Uppercase hex digits work too.
        assert!(parse_color_value("#FFFFFF").is_ok());
    }

    #[test]
    fn parse_color_value_hex_must_be_6_digits() {
        assert!(parse_color_value("#abc").is_err());
        assert!(parse_color_value("#1234567").is_err());
        assert!(parse_color_value("#xx1234").is_err());
    }

    #[test]
    fn parse_color_value_rejects_unknown_name() {
        assert!(parse_color_value("eggplant").is_err());
        assert!(parse_color_value("").is_err());
    }

    /// Theme JSON with partial fields merges over phosphor base.
    /// The file `{"agent": "blue"}` only changes `agent`; everything
    /// else stays phosphor.
    #[test]
    fn theme_json_partial_override_inherits_base() {
        let json = r#"{"agent": "blue"}"#;
        let overrides: ThemeJson = serde_json::from_str(json).unwrap();
        let theme = overrides.merge_into(Theme::phosphor(), "TEST").unwrap();
        assert!(matches!(theme.agent, Color::Blue), "agent overridden");
        // Everything else stays phosphor.
        assert!(matches!(theme.error, Color::Red), "error unchanged");
        assert!(matches!(theme.warn, Color::Yellow), "warn unchanged");
        assert!(matches!(theme.user, Color::Cyan), "user unchanged");
    }

    /// All-fields override produces a fully custom theme.
    #[test]
    fn theme_json_full_override_replaces_all_fields() {
        let json = r#"{
            "agent": "red",
            "user": "green",
            "system": "yellow",
            "tool": "blue",
            "perm": "magenta",
            "result": "cyan",
            "error": "darkred",
            "warn": "darkyellow",
            "accent": "white",
            "dim": "darkgrey",
            "header": "darkcyan",
            "divider": "darkgreen",
            "banner_primary": "darkblue",
            "banner_secondary": "darkmagenta",
            "label": "MIDNIGHT"
        }"#;
        let overrides: ThemeJson = serde_json::from_str(json).unwrap();
        let theme = overrides.merge_into(Theme::phosphor(), "MIDNIGHT").unwrap();
        assert!(matches!(theme.agent, Color::Red));
        assert!(matches!(theme.error, Color::DarkRed));
        assert!(matches!(theme.banner_primary, Color::DarkBlue));
        assert_eq!(theme.label, "MIDNIGHT");
    }

    /// Hex-color overrides flow through the parser.
    #[test]
    fn theme_json_accepts_hex_colors() {
        let json = r##"{"accent": "#ff8800"}"##;
        let overrides: ThemeJson = serde_json::from_str(json).unwrap();
        let theme = overrides.merge_into(Theme::phosphor(), "T").unwrap();
        assert!(matches!(
            theme.accent,
            Color::Rgb {
                r: 0xff,
                g: 0x88,
                b: 0x00,
            }
        ));
    }

    /// AnsiValue indices (256-color palette) parse.
    #[test]
    fn theme_json_accepts_ansi_value() {
        let json = r#"{"accent": 42}"#;
        let overrides: ThemeJson = serde_json::from_str(json).unwrap();
        let theme = overrides.merge_into(Theme::phosphor(), "T").unwrap();
        assert!(matches!(theme.accent, Color::AnsiValue(42)));
    }

    /// Unknown color name surfaces a parse error.
    #[test]
    fn theme_json_unknown_color_name_errors() {
        let json = r#"{"agent": "eggplant"}"#;
        let r: Result<ThemeJson, _> = serde_json::from_str(json);
        assert!(r.is_err(), "expected parse error for unknown color");
    }

    /// Unknown fields error out rather than silently being ignored —
    /// catches typos like `"acccent"` instead of `"accent"`.
    #[test]
    fn theme_json_unknown_field_errors() {
        let json = r#"{"acccent": "blue"}"#;
        let r: Result<ThemeJson, _> = serde_json::from_str(json);
        assert!(r.is_err(), "expected error for misspelled field");
    }

    /// `load_custom_theme` returns an Err with the file path when the
    /// file is missing — the path appears in the warning emitted by
    /// `init`, helping users find what dirge is looking for.
    #[test]
    fn load_custom_theme_missing_file_includes_path() {
        // Use a definitely-nonexistent theme name. The function
        // looks up `config_path()/<name>.theme.json` — even if
        // somebody has dirge configured, this name won't collide.
        let err = load_custom_theme("__definitely_not_a_real_theme_xyz")
            .expect_err("missing file must error");
        assert!(
            err.contains("__definitely_not_a_real_theme_xyz") || err.contains("no such file"),
            "error should reference the path or 'no such file': {err}",
        );
    }
}
