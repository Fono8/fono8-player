//! macOS / Windows tray through `tray-icon` (menu via `muda`).
//!
//! `tray-icon` objects must be created and updated on the UI thread, which is
//! where the model runs, so the animation advances from `Tray::tick`.

use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{logo_rgba, Pose, TrayCommand, TrayTexts};
use crate::meter::{Levels, Motion};

const ICON_SIZE: i32 = 32;
const FRAME_INTERVAL: Duration = Duration::from_millis(120);

struct Items {
    show: MenuItem,
    song: MenuItem,
    play_pause: MenuItem,
    previous: MenuItem,
    next: MenuItem,
    quit: MenuItem,
}

pub struct Tray {
    icon: TrayIcon,
    items: Items,
    playing: bool,
    phase: f32,
    last_frame: Instant,
    frames: Vec<Icon>,
    levels: Levels,
    motion: Motion,
}

fn icon(pose: Pose) -> Option<Icon> {
    Icon::from_rgba(logo_rgba(ICON_SIZE, pose), ICON_SIZE as u32, ICON_SIZE as u32).ok()
}

impl Tray {
    /// `levels` are the band levels of the local audio, drawn into the icon while playing.
    pub fn start(texts: TrayTexts, levels: Levels) -> Option<Tray> {
        let menu = Menu::new();
        let items = Items {
            show: MenuItem::with_id("show", &texts.show, true, None),
            song: MenuItem::with_id("song", &texts.song, false, None),
            play_pause: MenuItem::with_id("play_pause", &texts.play_pause, true, None),
            previous: MenuItem::with_id("previous", &texts.previous, true, None),
            next: MenuItem::with_id("next", &texts.next, true, None),
            quit: MenuItem::with_id("quit", &texts.quit, true, None),
        };
        menu.append(&items.show).ok()?;
        menu.append(&items.song).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&items.play_pause).ok()?;
        menu.append(&items.previous).ok()?;
        menu.append(&items.next).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&items.quit).ok()?;
        let icon = TrayIconBuilder::new()
            .with_id("fono8")
            .with_menu(Box::new(menu))
            .with_tooltip(&texts.tooltip)
            .with_icon(icon(Pose::Still)?)
            .with_menu_on_left_click(false)
            .build()
            .ok()?;
        Some(Tray {
            icon,
            items,
            playing: false,
            phase: 0.0,
            last_frame: Instant::now(),
            frames: Vec::new(),
            levels,
            motion: Motion::default(),
        })
    }

    pub fn poll(&self) -> Vec<TrayCommand> {
        let mut commands = Vec::new();
        for event in TrayIconEvent::receiver().try_iter() {
            match event {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => commands.push(TrayCommand::Show),
                _ => {}
            }
        }
        for event in MenuEvent::receiver().try_iter() {
            let command = match event.id.0.as_str() {
                "show" => Some(TrayCommand::Show),
                "play_pause" => Some(TrayCommand::PlayPause),
                "previous" => Some(TrayCommand::Previous),
                "next" => Some(TrayCommand::Next),
                "quit" => Some(TrayCommand::Quit),
                _ => None,
            };
            commands.extend(command);
        }
        commands
    }

    pub fn set_playing(&mut self, playing: bool) {
        if self.playing == playing {
            return;
        }
        self.playing = playing;
        if !playing {
            self.phase = 0.0;
            let _ = self.icon.set_icon(icon(Pose::Still));
        }
    }

    /// Advance the wave while playing; called from the model tick on the UI thread.
    pub fn tick(&mut self) {
        if !self.playing || self.last_frame.elapsed() < FRAME_INTERVAL {
            return;
        }
        self.last_frame = Instant::now();
        if let Some(levels) = self.motion.step(&self.levels, self.last_frame) {
            let _ = self.icon.set_icon(icon(Pose::Levels(levels)));
            return;
        }
        if self.frames.is_empty() {
            self.frames = (0..20).filter_map(|i| icon(Pose::Wave(i as f32 / 20.0))).collect();
        }
        if self.frames.is_empty() {
            return;
        }
        self.phase = (self.phase + 1.0 / 20.0) % 1.0;
        let index = ((self.phase * 20.0) as usize).min(self.frames.len() - 1);
        let _ = self.icon.set_icon(Some(self.frames[index].clone()));
    }

    pub fn set_texts(&self, texts: TrayTexts) {
        self.items.show.set_text(&texts.show);
        self.items.song.set_text(&texts.song);
        self.items.play_pause.set_text(&texts.play_pause);
        self.items.previous.set_text(&texts.previous);
        self.items.next.set_text(&texts.next);
        self.items.quit.set_text(&texts.quit);
        let _ = self.icon.set_tooltip(Some(&texts.tooltip));
    }

    pub fn set_song(&self, song: String, tooltip: String) {
        self.items.song.set_text(&song);
        let _ = self.icon.set_tooltip(Some(&tooltip));
    }

    pub fn shutdown(&self) {
        let _ = self.icon.set_visible(false);
    }
}
