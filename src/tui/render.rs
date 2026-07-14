use super::*;
use crate::state::DecisionKind;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags as TermFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let left = (max_chars - 1) / 2;
    let right = max_chars - 1 - left;
    let mut out = chars[..left].iter().collect::<String>();
    out.push('…');
    out.push_str(&chars[chars.len() - right..].iter().collect::<String>());
    out
}

pub(crate) fn short_container_id(container_id: &str) -> String {
    container_id.chars().take(12).collect()
}

mod overlays;
mod root;
mod sidebar;
mod terminal;

pub(crate) use overlays::*;
pub use root::render;
pub(crate) use root::render_scrollbar;
pub(crate) use sidebar::*;
pub(crate) use terminal::*;

mod panes {
    use super::*;

    #[path = "build.rs"]
    mod build;
    #[path = "text.rs"]
    mod text;

    pub(crate) use build::*;
    pub(crate) use text::*;
}

pub(crate) use panes::*;
