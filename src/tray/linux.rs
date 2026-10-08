//! Linux system tray: StatusNotifierItem over D-Bus via `ksni`.
//!
//! The tray runs on its own thread; menu activations are forwarded to the UI
//! through a channel that the model polls on each tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

use super::{logo_argb, Pose, TrayCommand, TrayTexts};
use crate::meter::{Levels, Motion};

struct TrayState {
    texts: TrayTexts,
    pose: Pose,
    /// The application icon, shown instead of the animated logo where the tray must stay still.
    app_icon: Option<Vec<ksni::Icon>>,
    commands: Sender<TrayCommand>,
}

impl ksni::Tray for TrayState {
    fn id(&self) -> String {
        "fono8".into()
    }

    fn title(&self) -> String {
        "Fono8".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if let Some(icon) = &self.app_icon {
            return icon.clone();
        }
        [22, 32, 48].into_iter().map(|size| ksni::Icon { width: size, height: size, data: logo_argb(size, self.pose) }).collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.texts.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.send(TrayCommand::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.texts.show.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.commands.send(TrayCommand::Show);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem { label: self.texts.song.clone(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem {
                label: self.texts.play_pause.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.commands.send(TrayCommand::PlayPause);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.texts.previous.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.commands.send(TrayCommand::Previous);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.texts.next.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.commands.send(TrayCommand::Next);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: self.texts.quit.clone(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.commands.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The application icon as StatusNotifierItem pixmaps (ARGB32, network byte order).
fn app_icon() -> Option<Vec<ksni::Icon>> {
    let icons: Vec<ksni::Icon> = crate::desktop::PNGS
        .iter()
        .filter(|(size, _)| matches!(size, 24 | 32 | 48 | 64))
        .filter_map(|(_, png)| image::load_from_memory_with_format(png, image::ImageFormat::Png).ok())
        .map(|image| {
            let rgba = image.to_rgba8();
            let (width, height) = rgba.dimensions();
            let data = rgba.pixels().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            ksni::Icon { width: width as i32, height: height as i32, data }
        })
        .collect();
    (!icons.is_empty()).then_some(icons)
}

/// `XDG_CURRENT_DESKTOP`, e.g. `ubuntu:GNOME` or `KDE`.
fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

/// Whether the tray icon may follow the music. GNOME shows tray icons through the
/// AppIndicator extension, which keeps per-update objects alive until the icon goes
/// away: after an hour of animation, quitting Fono8 froze the whole desktop for
/// seconds while GNOME Shell cleaned up. There the tray shows the application icon
/// and never changes it. `FONO8_TRAY_ANIMATION=1` or `0` overrides the choice.
fn animated(setting: Option<&str>, desktop: &str) -> bool {
    match setting {
        Some("1") => true,
        Some("0") => false,
        _ => !desktop.split(':').any(|name| name.eq_ignore_ascii_case("gnome")),
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<TrayState>,
    commands: Receiver<TrayCommand>,
    playing: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl Tray {
    /// Returns `None` when the desktop offers no StatusNotifier host.
    /// `levels` are the band levels of the local audio, drawn into the icon while playing.
    pub fn start(texts: TrayTexts, levels: Levels) -> Option<Tray> {
        let (sender, commands) = mpsc::channel();
        let animate = animated(std::env::var("FONO8_TRAY_ANIMATION").ok().as_deref(), &desktop());
        let app_icon = if animate { None } else { app_icon() };
        let state = TrayState { texts, pose: Pose::Still, app_icon, commands: sender };
        let handle = state.spawn().ok()?;
        let playing = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let animation_handle = handle.clone();
        let animation_playing = playing.clone();
        let animation_closed = closed.clone();
        if animate {
            std::thread::Builder::new()
                .name("fono8-tray-animation".into())
                .spawn(move || {
                    let mut phase = 0.0f32;
                    let mut motion = Motion::default();
                    let mut was_playing = false;
                    while !animation_closed.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(120));
                        let playing = animation_playing.load(Ordering::Relaxed);
                        let pose = match playing.then(|| motion.step(&levels, Instant::now())) {
                            None => Pose::Still,
                            Some(Some(levels)) => Pose::Levels(levels),
                            Some(None) => {
                                phase = (phase + 1.0 / 20.0) % 1.0;
                                Pose::Wave(phase)
                            }
                        };
                        if playing || was_playing {
                            animation_handle.update(|state| state.pose = pose);
                        }
                        was_playing = playing;
                    }
                })
                .ok()?;
        }
        Some(Tray { handle, commands, playing, closed })
    }

    pub fn poll(&self) -> Vec<TrayCommand> {
        self.commands.try_iter().collect()
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Relaxed);
    }

    /// Animation runs on its own thread here; nothing to do per tick.
    pub fn tick(&mut self) {}

    pub fn set_texts(&self, texts: TrayTexts) {
        self.handle.update(|state| state.texts = texts);
    }

    pub fn set_song(&self, song: String, tooltip: String) {
        self.handle.update(|state| {
            state.texts.song = song;
            state.texts.tooltip = tooltip;
        });
    }

    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.handle.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{animated, app_icon};

    #[test]
    fn the_tray_icon_is_still_on_gnome_unless_overridden() {
        assert!(!animated(None, "ubuntu:GNOME"));
        assert!(!animated(None, "GNOME"));
        assert!(!animated(None, "pop:gnome"));
        assert!(animated(None, "KDE"));
        assert!(animated(None, ""));
        assert!(animated(Some("1"), "ubuntu:GNOME"));
        assert!(!animated(Some("0"), "KDE"));
    }

    #[test]
    fn the_app_icon_is_decoded_for_the_tray() {
        let icons = app_icon().expect("embedded icons");
        assert_eq!(icons.iter().map(|i| i.width).collect::<Vec<_>>(), [24, 32, 48, 64]);
        let icon = &icons[0];
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        // ARGB: the corner of the rounded square is transparent, its centre is opaque.
        assert_eq!(icon.data[0], 0);
        let centre = ((icon.height / 2 * icon.width + icon.width / 2) * 4) as usize;
        assert_eq!(icon.data[centre], 255);
    }
}
