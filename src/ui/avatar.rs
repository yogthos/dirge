//! Bottom-left ASCII avatar.
//!
//! A tiny 3-row × 5-col face that lives in the left margin (cols
//! 0..5) of the bottom three terminal rows. It updates based on what
//! the agent is doing — thinking, speaking, running a tool, erroring,
//! resting — to give the chat a personable focal point and visible
//! activity feedback even when no tokens are streaming yet.
//!
//! Designed to fit inside the chat band's centering indent so it
//! never overlaps with chat content or the input prompt.

use crossterm::style::Color;

/// What the agent is currently doing. The renderer picks an ascii
/// face per state and draws it at the bottom-left of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum AvatarState {
    /// Nothing happening — neutral idle face.
    Idle,
    /// Model is thinking (reasoning tokens streaming).
    Thinking,
    /// Model is producing visible output (regular tokens streaming).
    Speaking,
    /// A read-family tool is active (read, grep, list_dir, find_files).
    Reading,
    /// A write-family tool is active (write, edit, apply_patch).
    Writing,
    /// A bash / shell tool is active.
    Bash,
    /// Permission alert or other thing demanding attention.
    Alert,
    /// Agent encountered an error.
    Error,
    /// Turn completed successfully.
    Done,
}

impl AvatarState {
    /// Choose an avatar state for a tool name. Maps well-known tool
    /// names to read/write/bash families; unknown tools default to
    /// the generic `Reading` face since most plugin / MCP tools are
    /// observational.
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "read" | "grep" | "find_files" | "list_dir" | "lsp" | "semantic" => Self::Reading,
            "write" | "edit" | "apply_patch" | "write_todo_list" => Self::Writing,
            "bash" | "shell" => Self::Bash,
            _ => Self::Reading,
        }
    }
}

/// Width of the avatar in terminal columns.
pub const AVATAR_W: usize = 5;
/// Height of the avatar in terminal rows.
pub const AVATAR_H: usize = 3;

/// Return three lines of ascii art for the given state + animation
/// tick. The `tick` boolean alternates between two slightly different
/// poses per state so the avatar visibly animates (eyes / mouth)
/// without going overboard.
pub fn art(state: AvatarState, tick: bool) -> [&'static str; AVATAR_H] {
    use AvatarState::*;
    match state {
        Idle => {
            if tick {
                [" ,-, ", "(o o)", " \\_/ "]
            } else {
                [" ,-, ", "(- -)", " \\_/ "]
            }
        }
        Thinking => {
            if tick {
                ["  ?  ", "(o ·)", " \\_/ "]
            } else {
                ["  ?  ", "(· o)", " \\_/ "]
            }
        }
        Speaking => {
            if tick {
                [" ,-, ", "(o o)", " \\o/ "]
            } else {
                [" ,-, ", "(o o)", " \\O/ "]
            }
        }
        Reading => {
            if tick {
                [" ,-, ", "[@ @]", " \\_/ "]
            } else {
                [" ,-, ", "[@ @]", " \\.. "]
            }
        }
        Writing => {
            if tick {
                [" ,-, ", "(>_<)", " \\_/ "]
            } else {
                [" ,-, ", "(-_-)", " \\_/ "]
            }
        }
        Bash => {
            if tick {
                ["[___]", "[$_$]", "[___]"]
            } else {
                ["[___]", "[$ $]", "[___]"]
            }
        }
        Alert => ["  !  ", "(O_O)", " /!\\ "],
        Error => [" ,-, ", "(x_x)", " /v\\ "],
        Done => [" ,-, ", "(^_^)", " \\_/ "],
    }
}

/// Color the avatar should render in for the given state. Default is
/// the active theme's agent tone; alerts and errors override to the
/// loud yellow/red of the theme so the user notices.
pub fn color(state: AvatarState) -> Color {
    use AvatarState::*;
    match state {
        Alert => crate::ui::theme::perm(),
        Error => crate::ui::theme::error(),
        Done => crate::ui::theme::accent(),
        _ => crate::ui::theme::agent(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every state must produce three lines exactly `AVATAR_W` cols
    /// wide. A typo'd asymmetry would visually wobble the face.
    #[test]
    fn every_state_has_uniform_dimensions() {
        let states = [
            AvatarState::Idle,
            AvatarState::Thinking,
            AvatarState::Speaking,
            AvatarState::Reading,
            AvatarState::Writing,
            AvatarState::Bash,
            AvatarState::Alert,
            AvatarState::Error,
            AvatarState::Done,
        ];
        for state in states {
            for tick in [false, true] {
                let lines = art(state, tick);
                assert_eq!(lines.len(), AVATAR_H, "{:?} wrong row count", state);
                for (i, line) in lines.iter().enumerate() {
                    assert_eq!(
                        line.chars().count(),
                        AVATAR_W,
                        "{:?} tick={} row {} is {:?}",
                        state,
                        tick,
                        i,
                        line,
                    );
                }
            }
        }
    }

    /// Tool-name → state mapping covers the common families.
    #[test]
    fn tool_name_maps_to_state() {
        assert_eq!(AvatarState::from_tool_name("read"), AvatarState::Reading);
        assert_eq!(AvatarState::from_tool_name("grep"), AvatarState::Reading);
        assert_eq!(AvatarState::from_tool_name("edit"), AvatarState::Writing);
        assert_eq!(AvatarState::from_tool_name("write"), AvatarState::Writing);
        assert_eq!(AvatarState::from_tool_name("bash"), AvatarState::Bash);
        // Unknown tools fall back to Reading (observational default).
        assert_eq!(
            AvatarState::from_tool_name("mcp_some_tool"),
            AvatarState::Reading
        );
    }
}
