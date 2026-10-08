//! GPUI user interface: assets, theme, widgets and the main window.

pub mod main_view;
pub mod shell;
pub mod text_input;
pub mod theme;
pub mod widgets;

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub use main_view::open_main_window;
pub use shell::Shell;

/// SVG icons embedded in the binary; GPUI loads them through this source.
pub struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"), include_bytes!(concat!("../../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "home",
    "close",
    "trash",
    "queue",
    "pin",
    "clock",
    "drag",
    "youtube",
    "spotify",
    "tidal",
    "settings",
    "heart",
    "heart-filled",
    "play",
    "pause",
    "next",
    "previous",
    "folder",
    "plus",
    "search",
    "music",
    "list",
    "shuffle",
    "repeat",
    "volume",
    "muted",
    "tray",
    "mini",
    "more",
    "refresh",
    "check",
    "chevron",
    "resize",
    "cast",
    "wordmark-cyan",
    "wordmark-purple",
);

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().filter(|(name, _)| name.starts_with(path)).map(|(name, _)| SharedString::from(*name)).collect())
    }
}
