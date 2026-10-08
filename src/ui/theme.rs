//! Freeven-inspired palette shared by every element.

use gpui::{rgb, rgba, Rgba};

pub const BACKGROUND: Rgba = Rgba { r: 0x07 as f32 / 255.0, g: 0x10 as f32 / 255.0, b: 0x1e as f32 / 255.0, a: 1.0 };
pub const SURFACE: Rgba = Rgba { r: 0x0e as f32 / 255.0, g: 0x1a as f32 / 255.0, b: 0x2c as f32 / 255.0, a: 1.0 };
pub const ELEVATED: Rgba = Rgba { r: 0x15 as f32 / 255.0, g: 0x24 as f32 / 255.0, b: 0x3a as f32 / 255.0, a: 1.0 };
pub const BORDER: Rgba = Rgba { r: 0x24 as f32 / 255.0, g: 0x33 as f32 / 255.0, b: 0x4b as f32 / 255.0, a: 1.0 };
pub const TEXT: Rgba = Rgba { r: 0xe8 as f32 / 255.0, g: 0xf0 as f32 / 255.0, b: 0xff as f32 / 255.0, a: 1.0 };
pub const MUTED: Rgba = Rgba { r: 0x9b as f32 / 255.0, g: 0xae as f32 / 255.0, b: 0xcb as f32 / 255.0, a: 1.0 };
pub const ACCENT: Rgba = Rgba { r: 0x3d as f32 / 255.0, g: 0xe8 as f32 / 255.0, b: 0xf7 as f32 / 255.0, a: 1.0 };
pub const PURPLE: Rgba = Rgba { r: 0x9b as f32 / 255.0, g: 0x6d as f32 / 255.0, b: 0xff as f32 / 255.0, a: 1.0 };

/// Text sizes in px, smallest first. Lists and cards use the same scale.
/// Badges: source labels, account lines under buttons.
pub const TEXT_TINY: f32 = 11.0;
/// Second lines: artists, track counts, hints.
pub const TEXT_SMALL: f32 = 12.0;
/// List columns, notes and status lines.
pub const TEXT_DETAIL: f32 = 13.0;
/// Titles in lists and cards, and the window default.
pub const TEXT_BODY: f32 = 14.0;

pub fn root_translucent() -> Rgba {
    rgba(0x07101eed)
}

pub fn panel() -> Rgba {
    rgba(0x0c1829b3)
}

pub fn player_bar() -> Rgba {
    rgba(0x101c30b3)
}

pub fn queue_panel() -> Rgba {
    rgb(0x101c2e)
}

pub fn selected_row() -> Rgba {
    rgb(0x273149)
}

pub fn hover_row() -> Rgba {
    rgb(0x17273d)
}

pub fn stripe_row() -> Rgba {
    rgba(0x00000018)
}

pub fn selected_button() -> Rgba {
    rgb(0x243347)
}

pub fn accent_pressed() -> Rgba {
    rgb(0x20c9db)
}

pub fn search_field() -> Rgba {
    rgb(0x152136)
}

pub fn playlist_selected() -> Rgba {
    rgb(0x252a48)
}

pub fn queue_current() -> Rgba {
    rgb(0x20394a)
}

pub fn pinned_card() -> Rgba {
    rgb(0x1b2840)
}

pub fn slider_track() -> Rgba {
    rgb(0x30415c)
}

pub fn menu_bg() -> Rgba {
    rgb(0x0b192e)
}

pub fn menu_border() -> Rgba {
    rgb(0x263d5e)
}

pub fn menu_hover() -> Rgba {
    rgb(0x30264f)
}

pub fn overlay() -> Rgba {
    rgba(0x00000080)
}

pub fn queue_overlay() -> Rgba {
    rgba(0x00000060)
}

pub fn tooltip_bg() -> Rgba {
    rgb(0x122440)
}

pub fn tooltip_border() -> Rgba {
    rgb(0x53628c)
}

pub const COVER_VARIANTS: [u32; 4] = [0x214862, 0x433768, 0x204c4d, 0x313b69];
