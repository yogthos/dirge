//! Configurable key bindings for the global "command" keys (VSCode-style).
//!
//! The TUI's agent-control keys (toggle reasoning, scroll, chat
//! navigation, kill-subagent) resolve through a [`Keymap`] that maps a
//! key chord → [`KeyAction`]. Built-in defaults reproduce the historical
//! bindings; the user's `keybindings` config (an array of
//! `{ key, command }`) overrides them per chord, exactly like a VSCode
//! `keybindings.json`.
//!
//! Out of scope (kept fixed): the input-editor's text-editing keys
//! (Ctrl+A/E/W, kill-ring, word motion, history) and the universal
//! cancel/interrupt gesture (Ctrl+C / Ctrl+D / Esc) — the latter must
//! always be available as the panic button.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeybindingConfig;

/// A rebindable global command. Each maps to a stable `command` string
/// used in the config and to a set of default chords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    ToggleReasoning,
    /// Expand on demand: the buffered thinking (while the agent is
    /// thinking), else reprint the last collapsed tool result in full.
    Expand,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    NextChat,
    PrevChat,
    CloseChat,
    KillSubagent,
}

impl KeyAction {
    /// All actions, with their config command name and default chords.
    /// Single source of truth for both the default keymap and the
    /// command-name lookup / docs.
    pub const ALL: &'static [(KeyAction, &'static str, &'static [(KeyCode, KeyModifiers)])] = &[
        (
            KeyAction::ToggleReasoning,
            "toggle_reasoning",
            &[(KeyCode::Char('r'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::Expand,
            "expand",
            &[(KeyCode::Char('o'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::ScrollPageUp,
            "scroll_page_up",
            &[(KeyCode::PageUp, KeyModifiers::NONE)],
        ),
        (
            KeyAction::ScrollPageDown,
            "scroll_page_down",
            &[(KeyCode::PageDown, KeyModifiers::NONE)],
        ),
        (
            KeyAction::ScrollToTop,
            "scroll_to_top",
            &[(KeyCode::Home, KeyModifiers::NONE)],
        ),
        (
            KeyAction::ScrollToBottom,
            "scroll_to_bottom",
            &[(KeyCode::End, KeyModifiers::NONE)],
        ),
        (
            KeyAction::NextChat,
            "next_chat",
            &[(KeyCode::Char('n'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::PrevChat,
            "prev_chat",
            &[(KeyCode::Char('p'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::CloseChat,
            "close_chat",
            &[(KeyCode::Char('x'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::KillSubagent,
            "kill_subagent",
            &[(KeyCode::Char('k'), KeyModifiers::CONTROL)],
        ),
    ];

    /// Resolve a config command name (case-insensitive, `-`/`_` agnostic)
    /// to an action. `None` for unknown commands.
    pub fn from_command(name: &str) -> Option<KeyAction> {
        let norm = name.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .iter()
            .find(|(_, cmd, _)| *cmd == norm)
            .map(|(a, _, _)| *a)
    }

    /// Comma-separated list of every valid command name (for help /
    /// warning text).
    pub fn command_list() -> String {
        Self::ALL
            .iter()
            .map(|(_, c, _)| *c)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Resolves key chords to [`KeyAction`]s: built-in defaults plus the
/// user's per-chord overrides.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    map: HashMap<(KeyCode, KeyModifiers), KeyAction>,
}

impl Keymap {
    /// The built-in keymap (no config applied).
    pub fn defaults() -> Self {
        let mut map = HashMap::new();
        for (action, _, chords) in KeyAction::ALL {
            for chord in *chords {
                map.insert(*chord, *action);
            }
        }
        Self { map }
    }

    /// Build the keymap from the optional `keybindings` config, layered
    /// over the defaults. Returns the keymap plus any warnings (invalid
    /// chord / unknown command) for the UI to surface. A command of
    /// `none` / `unbind` removes the chord (so a user can disable a
    /// default binding).
    pub fn from_config(bindings: Option<&[KeybindingConfig]>) -> (Self, Vec<String>) {
        let mut km = Self::defaults();
        let mut warnings = Vec::new();
        for b in bindings.unwrap_or(&[]) {
            let Some(chord) = parse_chord(&b.key) else {
                warnings.push(format!("keybindings: unrecognized key {:?}", b.key));
                continue;
            };
            let cmd = b.command.trim().to_ascii_lowercase().replace('-', "_");
            if matches!(cmd.as_str(), "none" | "noop" | "unbind" | "") {
                km.map.remove(&chord);
                continue;
            }
            match KeyAction::from_command(&cmd) {
                Some(action) => {
                    km.map.insert(chord, action);
                }
                None => warnings.push(format!(
                    "keybindings: unknown command {:?} for key {:?} (valid: {})",
                    b.command,
                    b.key,
                    KeyAction::command_list()
                )),
            }
        }
        (km, warnings)
    }

    /// The action bound to `key`, if any. Matches modifiers exactly.
    pub fn resolve(&self, key: &KeyEvent) -> Option<KeyAction> {
        self.map.get(&(key.code, key.modifiers)).copied()
    }
}

/// Parse a chord string like `ctrl-r`, `pageup`, `ctrl-shift-x`,
/// `home`, `f5` into a `(KeyCode, KeyModifiers)`. Case-insensitive,
/// `-`-separated, modifiers before the key. Returns `None` on a
/// malformed spec. (A standalone copy of the plugin chord grammar so
/// this module stays available without the `plugin` feature.)
pub fn parse_chord(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let spec = spec.trim().to_ascii_lowercase();
    if spec.is_empty() {
        return None;
    }
    let parts: Vec<&str> = spec.split(['-', '+']).filter(|s| !s.is_empty()).collect();
    let (key_part, mod_parts) = parts.split_last()?;
    let mut modifiers = KeyModifiers::NONE;
    for m in mod_parts {
        match *m {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let code = match *key_part {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pagedn" => KeyCode::PageDown,
        f if f.starts_with('f') && f.len() >= 2 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = f[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return None,
    };
    Some((code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(key: &str, command: &str) -> KeybindingConfig {
        KeybindingConfig {
            key: key.to_string(),
            command: command.to_string(),
        }
    }
    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn defaults_resolve() {
        let km = Keymap::defaults();
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(KeyAction::ScrollPageUp)
        );
        // A plain char / unbound chord resolves to nothing.
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn parse_chord_forms() {
        assert_eq!(
            parse_chord("ctrl-r"),
            Some((KeyCode::Char('r'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("Ctrl+T"),
            Some((KeyCode::Char('t'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("pageup"),
            Some((KeyCode::PageUp, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("ctrl-shift-x"),
            Some((
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
        );
        assert_eq!(parse_chord("f5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(parse_chord("boguskey"), None);
        assert_eq!(parse_chord("ctrl-"), None);
        assert_eq!(parse_chord("f99"), None);
    }

    #[test]
    fn override_rebinds_and_keeps_other_defaults() {
        // Rebind toggle-reasoning to Ctrl+T.
        let (km, warns) = Keymap::from_config(Some(&[cfg("ctrl-t", "toggle_reasoning")]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        // The default Ctrl+R still toggles (adding a binding doesn't drop
        // the default), and an unrelated default is intact.
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(KeyAction::NextChat)
        );
    }

    #[test]
    fn override_on_an_occupied_chord_replaces_it() {
        // Binding Ctrl+R to next_chat takes Ctrl+R away from toggle.
        let (km, warns) = Keymap::from_config(Some(&[cfg("ctrl-r", "next_chat")]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::NextChat)
        );
    }

    #[test]
    fn unbind_removes_a_default() {
        let (km, _) = Keymap::from_config(Some(&[cfg("ctrl-r", "none")]));
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn invalid_chord_and_unknown_command_warn() {
        let (_, warns) = Keymap::from_config(Some(&[
            cfg("kaboom", "toggle_reasoning"),
            cfg("ctrl-y", "do_a_barrel_roll"),
        ]));
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(warns[0].contains("unrecognized key"));
        assert!(warns[1].contains("unknown command"));
    }
}
