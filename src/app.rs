//! The application model: library, queue, settings and the view state shared by
//! the main window, the tray and dialogs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, AppContext, Bounds, Context, Entity, Pixels, Point, Size, Subscription, WindowHandle};
use serde_json::{json, Value};

use crate::artwork::Artwork;
use crate::audio::PlaybackState;
use crate::cast::{CastOutput, CastStatus};
use crate::discover::{interleave, Discover, RowRef};
use crate::i18n::{Arg, Message, Translator};
use crate::library::{self, format_duration, is_remote_path, is_youtube_path, FolderMetadataProposal, Library, Track};
use crate::playback::{Playback, PlaybackEvent, Remote};
use crate::services::{AccountView, Button, Row, Service, ServiceAction};
use crate::sleep_timer::SleepTimer;
use crate::spotify::api::Profile as SpotifyProfile;
use crate::spotify::client::{Request as SpotifyRequest, Spotify, Update as SpotifyUpdate};
use crate::spotify::panel::{Item as SpotifyItem, More as SpotifyMore, Panel as SpotifyPanel};
use crate::spotify::player::SpotifyPlayer;
use crate::tidal::api::Profile as TidalProfile;
use crate::tidal::client::Tidal;
use crate::transfer::{Stage, Transfer};
use crate::tray::{Tray, TrayCommand, TrayTexts};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::Shell;
use crate::youtube::provider::{Operation, Outcome};
use crate::youtube::{HelperEvent, Request, ResultItem, Session, YouTube};

mod tidal_import;

pub use tidal_import::ImportMode;
use tidal_import::TidalRow;

/// Diagnostic logging to stderr, enabled with `FONO8_DEBUG=1`.
pub fn debug(message: impl FnOnce() -> String) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("FONO8_DEBUG").is_some()) {
        eprintln!("[fono8] {}", message());
    }
}

pub const FULL_SIZE: (f32, f32) = (900.0, 640.0);
pub const COMPACT_SIZE: (f32, f32) = (440.0, 210.0);
pub const MIN_COMPACT: (f32, f32) = (420.0, 210.0);
pub const MAX_COMPACT: (f32, f32) = (640.0, 280.0);

/// The note shown when a streaming track plays on this computer during a Cast session.
pub fn cast_soon_key(remote: Remote) -> &'static str {
    match remote {
        Remote::YouTube => "yt_cast_soon",
        Remote::Spotify => "spotify_cast_soon",
    }
}

/// Whether GPUI drives the window through Wayland (its own rule in `guess_compositor`).
fn on_wayland() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os("ZED_HEADLESS").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty())
}

/// Where to open the window. Wayland clients cannot place their windows, and GPUI 0.2
/// reuses the origin as the window geometry offset in `Window::resize`: an origin of
/// (120, 80) made GNOME shrink a 440x210 mini window to 320x130 at the next configure
/// (e.g. when the window lost focus). So the origin is zero there.
fn window_origin(saved: Option<Point<Pixels>>, wayland: bool) -> Point<Pixels> {
    if wayland {
        Point::default()
    } else {
        saved.unwrap_or(Point { x: px(120.0), y: px(80.0) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Library,
    Discover,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    Accounts,
    General,
}

#[derive(Clone, Debug)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub folder: Option<String>,
    pub count: usize,
    pub cover_path: Option<String>,
    pub pinned: bool,
    /// The special "My favorites" playlist: always first, cannot be renamed or deleted.
    pub favorite: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    PlaySelected,
    EnqueueSelected,
    EditMetadataSelected,
    MoveUp,
    MoveDown,
    RemoveSelected,
    EnqueuePlaylist(i64),
    RenamePlaylist(i64),
    ExportPlaylist(i64),
    EditPlaylistMetadata(i64),
    Rescan(String),
    DeletePlaylist(i64),
    RemoveFromQueue(usize),
    PlayPath(String),
    EnqueuePath(String),
    ToggleTranslucent,
    ToggleCompact,
    ResetLayout,
    Language(String),
    DiscoverTarget(Option<i64>),
    SearchOnline(String),
    AddPathsTo(Vec<String>, i64),
    ToggleFavorites(Vec<String>),
    OpenSettings,
    Quit,
    Back,
}

#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item { label: String, action: MenuAction, enabled: bool, checked: Option<bool> },
    Submenu { label: String, items: Vec<MenuEntry>, enabled: bool },
    Separator,
}

#[derive(Clone, Debug)]
pub struct ContextMenu {
    pub position: Point<Pixels>,
    pub items: Vec<MenuEntry>,
    pub parent: Option<Vec<MenuEntry>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    NewPlaylist,
    RenamePlaylist(i64),
    SaveQueue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmKind {
    DeletePlaylist(i64),
}

pub enum Dialog {
    Input {
        kind: InputKind,
        title: &'static str,
        label: &'static str,
        note: Option<&'static str>,
        input: Entity<TextInput>,
        _subscription: Subscription,
    },
    Confirm {
        kind: ConfirmKind,
        title: &'static str,
        body: &'static str,
    },
    SleepTimer {
        hours: Entity<TextInput>,
        minutes: Entity<TextInput>,
        _subscriptions: Vec<Subscription>,
    },
    Metadata {
        proposals: Vec<FolderMetadataProposal>,
        index: usize,
        artist: Entity<TextInput>,
        album: Entity<TextInput>,
        error: bool,
        _subscriptions: Vec<Subscription>,
    },
    Message {
        title: String,
        body: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragList {
    Tracks,
    Queue,
}

#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub list: DragList,
    pub source: usize,
    pub target: usize,
    pub y: f32,
}

enum ScanMessage {
    Progress(usize),
    Done(String, Vec<Track>, Vec<String>),
    Failed(Message),
    Cancelled,
}

struct ScanJob {
    cancel: Arc<AtomicBool>,
    receiver: Receiver<ScanMessage>,
}

#[derive(Clone, Debug)]
pub struct WindowState {
    pub full_size: Size<Pixels>,
    pub compact_size: Size<Pixels>,
    pub origin: Option<Point<Pixels>>,
    pub maximized: bool,
}

pub struct Fono8 {
    pub library: Library,
    pub i18n: Translator,
    pub playback: Playback,
    pub artwork: Artwork,
    pub sleep_timer: SleepTimer,
    pub tray: Option<Tray>,
    pub cast: CastOutput,
    pub cast_open: bool,
    pub youtube: YouTube,
    pub discover: Discover,
    /// The id of the "My favorites" playlist and the tracks in it.
    pub favorites: Option<i64>,
    pub favorite_paths: HashSet<String>,
    /// The Discover search and playlist-link fields.
    pub discover_inputs: Option<[Entity<TextInput>; 2]>,
    discover_subscriptions: Vec<Subscription>,
    pub settings_tab: SettingsTab,
    /// The Spotify worker, started once a Client ID is known.
    pub spotify: Option<Spotify>,
    /// The signed-in Spotify account.
    pub spotify_profile: Option<SpotifyProfile>,
    spotify_player: Option<SpotifyPlayer>,
    /// Measures the Spotify browser's audio for the logo while Spotify is the active source.
    #[cfg(target_os = "linux")]
    spotify_tap: Option<crate::stream_tap::StreamTap>,
    pub spotify_panel: SpotifyPanel,
    /// The TIDAL worker (import only: TIDAL is never played), its account and Discover tab.
    tidal: Option<Tidal>,
    tidal_profile: Option<TidalProfile>,
    tidal_client_input: Option<Entity<TextInput>>,
    tidal_account_message: Message,
    tidal_rows: Vec<TidalRow>,
    tidal_busy: bool,
    tidal_message: Message,
    /// The TIDAL import being matched, if any.
    transfer: Option<Transfer>,
    /// The Client ID field in Settings → Accounts.
    spotify_client_input: Option<Entity<TextInput>>,
    /// The last sign-in related Spotify message, shown on the account card.
    spotify_account_message: Message,
    /// The next playlists reply is a first page (replaces the list).
    spotify_playlists_first: bool,
    pub page: Page,
    pub current_playlist: Option<i64>,
    /// "All tracks" narrowed to one artist (clicked on a track).
    pub artist_filter: Option<String>,
    pub query: String,
    pub selected: HashSet<String>,
    pub selection_anchor: usize,
    pub focused_path: Option<String>,
    pub compact: bool,
    pub translucent: bool,
    pub queue_open: bool,
    pub busy: bool,
    pub status: Message,
    pub current_track: Option<Track>,
    pub tracks: Vec<Track>,
    pub playlists: Vec<PlaylistRow>,
    pub recent: Vec<Track>,
    pub menu: Option<ContextMenu>,
    pub dialog: Option<Dialog>,
    pub drag: Option<Drag>,
    pub window: Option<WindowHandle<Shell>>,
    pub window_state: WindowState,
    pub quitting: bool,
    pub font_family: String,
    scanner: Option<ScanJob>,
    pending_window_save: Option<Instant>,
    last_tray_song: String,
    last_cast_message: Message,
    /// The streaming service whose track plays while Cast is active (it stays on this computer).
    remote_beside_cast: Option<Remote>,
}

fn validate_size(value: Option<&Value>, default: (f32, f32)) -> Size<Pixels> {
    if let Some(items) = value.and_then(Value::as_array) {
        if items.len() == 2 {
            let w = items[0].as_f64().unwrap_or(0.0);
            let h = items[1].as_f64().unwrap_or(0.0);
            if w > 0.0 && w <= 32767.0 && h > 0.0 && h <= 32767.0 {
                return Size { width: px(w as f32), height: px(h as f32) };
            }
        }
    }
    Size { width: px(default.0), height: px(default.1) }
}

impl Fono8 {
    pub fn new(library: Library, language: Option<String>, cx: &mut Context<Self>) -> Self {
        let preference = language.unwrap_or_else(|| library.setting_string("language").unwrap_or_else(|| "auto".into()));
        let i18n = Translator::new(&preference);
        let mut playback = Playback::new();
        playback.set_volume(library.setting_f64("volume", 0.65));
        playback.shuffle = library.setting_bool("shuffle", false);
        playback.repeat = library.setting_bool("repeat", false);
        let translucent = library.setting_bool("translucent", true);
        let queue_open = library.setting_bool("queue_open", false);
        let state = library.setting("window_state").unwrap_or(Value::Null);
        let window_state = WindowState {
            full_size: validate_size(state.get("full_size"), FULL_SIZE),
            compact_size: validate_size(state.get("compact_size"), COMPACT_SIZE),
            origin: state.get("rs_origin").and_then(Value::as_array).and_then(|items| {
                if items.len() == 2 {
                    Some(Point { x: px(items[0].as_f64()? as f32), y: px(items[1].as_f64()? as f32) })
                } else {
                    None
                }
            }),
            maximized: state.get("rs_maximized").and_then(Value::as_bool).unwrap_or(false),
        };
        let compact = state.get("compact").and_then(Value::as_bool).unwrap_or(false);
        let font_family = pick_font(cx);
        let cast = CastOutput::new(playback.engine.capture_sink());
        let mut this = Fono8 {
            library,
            i18n,
            cast,
            cast_open: false,
            youtube: YouTube::default(),
            discover: Discover::default(),
            favorites: None,
            favorite_paths: HashSet::new(),
            discover_inputs: None,
            discover_subscriptions: Vec::new(),
            settings_tab: SettingsTab::Accounts,
            spotify: None,
            spotify_profile: None,
            spotify_player: None,
            #[cfg(target_os = "linux")]
            spotify_tap: None,
            spotify_panel: SpotifyPanel::default(),
            tidal: None,
            tidal_profile: None,
            tidal_client_input: None,
            tidal_account_message: Message::new("tidal_status_signed_out"),
            tidal_rows: Vec::new(),
            tidal_busy: false,
            tidal_message: Message::new("tidal_intro"),
            transfer: None,
            spotify_client_input: None,
            spotify_account_message: Message::new("spotify_status_signed_out"),
            spotify_playlists_first: true,
            playback,
            artwork: Artwork::new(),
            sleep_timer: SleepTimer::default(),
            tray: None,
            page: Page::Home,
            current_playlist: None,
            artist_filter: None,
            query: String::new(),
            selected: HashSet::new(),
            selection_anchor: 0,
            focused_path: None,
            compact,
            translucent,
            queue_open,
            busy: false,
            status: Message::new("ready"),
            current_track: None,
            tracks: Vec::new(),
            playlists: Vec::new(),
            recent: Vec::new(),
            menu: None,
            dialog: None,
            drag: None,
            window: None,
            window_state,
            quitting: false,
            font_family,
            scanner: None,
            pending_window_save: None,
            last_tray_song: String::new(),
            last_cast_message: Message::new("cast_local"),
            remote_beside_cast: None,
        };
        this.youtube.forget_pending = this.library.setting_bool("yt_forget_pending", false);
        this.tray = Tray::start(this.tray_texts(), this.playback.engine.levels());
        debug(|| format!("tray available: {}, font: {}", this.tray.is_some(), this.font_family));
        this.playback.preview_dir = this.library.path.parent().map(|dir| dir.join("tidal-previews"));
        this.ensure_favorites();
        this.refresh_playlists(None, true);
        if let Some(client_id) = this.library.setting_string("spotify_client_id").filter(|id| !id.is_empty()) {
            this.spotify_client().send(SpotifyRequest::Restore { client_id });
        }
        if let Some(client_id) = this.library.setting_string("tidal_client_id").filter(|id| !id.is_empty()) {
            this.tidal_client().send(crate::tidal::client::Request::Restore { client_id });
        }
        this.start_ticker(cx);
        this
    }

    // ----- translations -------------------------------------------------

    pub fn t(&self, key: &str) -> String {
        self.i18n.t(key)
    }

    pub fn text(&self, key: &str, values: &[(&str, Arg)]) -> String {
        self.i18n.text(key, values)
    }

    pub fn status_text(&self) -> String {
        self.i18n.message(&self.status)
    }

    pub fn set_status(&mut self, message: Message) {
        self.status = message;
    }

    fn tray_texts(&self) -> TrayTexts {
        TrayTexts {
            tooltip: self.t("tray_tooltip"),
            show: self.t("show_fono8"),
            song: self.current_track.as_ref().map(|t| t.title.chars().take(70).collect()).unwrap_or_else(|| self.t("idle_tray")),
            play_pause: self.t("play_pause"),
            previous: self.t("previous"),
            next: self.t("next"),
            quit: self.t("quit"),
        }
    }

    pub fn change_language(&mut self, preference: &str) {
        self.i18n.set_language(preference);
        let preference = self.i18n.preference.clone();
        self.library.set_setting("language", json!(preference));
        if let Some(tray) = &self.tray {
            tray.set_texts(self.tray_texts());
        }
        if let Some(session) = &self.youtube.session {
            session.retranslate(&self.i18n);
        }
        self.refresh_playlists(None, true);
    }

    // ----- periodic work ------------------------------------------------

    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let alive = this.update(cx, |model, cx| {
                model.tick(cx);
                !model.quitting
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        self.playback.tick();
        #[cfg(target_os = "linux")]
        {
            let spotify = self.playback.active_remote() == Some(Remote::Spotify);
            if spotify != self.spotify_tap.is_some() {
                self.spotify_tap = spotify
                    .then(|| crate::stream_tap::StreamTap::start(&self.spotify_profile_dir(), self.playback.engine.levels()));
            }
        }
        for event in self.playback.take_events() {
            changed = true;
            match event {
                PlaybackEvent::TrackChanged(path) => self.track_changed(&path),
                PlaybackEvent::NeedsPreview(path) => self.fetch_preview(&path),
                PlaybackEvent::NeedsYouTube(path) => {
                    // Start the engine and play the same item; ensure_youtube reports its own failure.
                    if self.playback.current() == Some(path.as_str()) && self.ensure_youtube() {
                        self.playback.retry_current();
                    }
                }
                PlaybackEvent::QueueChanged => {}
                PlaybackEvent::StateChanged(state) => {
                    if let Some(tray) = self.tray.as_mut() {
                        tray.set_playing(state == PlaybackState::Playing);
                    }
                }
                PlaybackEvent::Error(message) => {
                    let text = self.i18n.message(&message);
                    debug(|| format!("playback error: {text}"));
                    self.set_status(Message::new("playback_error").with("message", text));
                }
            }
        }
        if self.playback.current().is_some() {
            changed = true; // position moves while a track is loaded
        }
        if self.sleep_timer.tick() {
            self.playback.stop();
            self.set_status(Message::new("sleep_timer_finished"));
            changed = true;
        } else if self.sleep_timer.active() {
            changed = true;
        }
        if self.artwork.poll() {
            changed = true;
        }
        if let Some(job) = &self.scanner {
            let mut finished = false;
            let messages: Vec<ScanMessage> = job.receiver.try_iter().collect();
            for message in messages {
                changed = true;
                match message {
                    ScanMessage::Progress(count) => {
                        self.set_status(Message::new("scan_progress").with("tracks", Message::count("tracks_count", count)));
                    }
                    ScanMessage::Done(folder, tracks, errors) => {
                        finished = true;
                        debug(|| format!("scan done: {} track(s), {} error(s) in {folder}", tracks.len(), errors.len()));
                        self.scan_complete(&folder, tracks, errors, cx);
                    }
                    ScanMessage::Failed(message) => {
                        finished = true;
                        debug(|| format!("scan failed: {message:?}"));
                        let text = self.i18n.message(&message);
                        self.set_status(Message::new("scan_error").with("error", text));
                    }
                    ScanMessage::Cancelled => finished = true,
                }
            }
            if finished {
                self.scanner = None;
                self.busy = false;
            }
        }
        if let Some(tray) = self.tray.as_mut() {
            tray.tick();
        }
        let commands: Vec<TrayCommand> = self.tray.as_ref().map(|tray| tray.poll()).unwrap_or_default();
        for command in commands {
            changed = true;
            match command {
                TrayCommand::Show => self.show_window(cx),
                TrayCommand::PlayPause => self.toggle_play(),
                TrayCommand::Previous => self.playback.previous(),
                TrayCommand::Next => self.playback.next(false),
                TrayCommand::Quit => self.quit(cx),
            }
        }
        if self.poll_youtube_login() {
            changed = true;
        }
        if self.poll_spotify(cx) {
            changed = true;
        }
        if self.poll_tidal(cx) {
            changed = true;
        }
        if self.transfer.as_ref().is_some_and(|t| t.stage == Stage::Pending) {
            self.transfer_step();
            changed = true;
        }
        let helper_events: Vec<HelperEvent> = self.youtube.session.as_mut().map(|s| s.poll()).unwrap_or_default();
        for event in helper_events {
            changed = true;
            self.helper_event(event);
        }
        if self.cast.poll() {
            changed = true;
            self.playback.casting = self.cast.status().state != crate::cast::CastState::Local;
            let status = self.cast.status();
            if status.message != self.last_cast_message {
                self.last_cast_message = status.message.clone();
                self.set_status(status.message);
            }
        }
        // Cast only carries audio decoded by Fono8; say so once when a streaming track meets Cast.
        let remote_beside_cast = self.remote_beside_cast();
        if let Some(remote) = remote_beside_cast.filter(|_| remote_beside_cast != self.remote_beside_cast) {
            self.set_status(Message::new(cast_soon_key(remote)));
            changed = true;
        }
        self.remote_beside_cast = remote_beside_cast;
        if let Some(when) = self.pending_window_save {
            if when.elapsed() >= Duration::from_millis(250) {
                self.pending_window_save = None;
                self.save_window_state();
            }
        }
        if changed {
            cx.notify();
        }
    }

    // ----- window lifecycle ---------------------------------------------

    pub fn show_window(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.window {
            // macOS hides the whole application; elsewhere the window was minimized.
            // Un-minimize where the platform allows it (X11; Wayland needs an activation token).
            if window.update(cx, |_, window, _| window.activate_window()).is_ok() {
                cx.activate(true);
                return;
            }
            self.window = None;
        }
        let model = cx.entity();
        cx.defer(move |cx| crate::ui::open_main_window(model, cx));
    }

    pub fn window_bounds(&self) -> Bounds<Pixels> {
        let size = if self.compact { self.window_state.compact_size } else { self.window_state.full_size };
        Bounds { origin: window_origin(self.window_state.origin, on_wayland()), size }
    }

    pub fn record_window_bounds(&mut self, bounds: Bounds<Pixels>, maximized: bool) {
        if self.quitting {
            return;
        }
        if !maximized {
            if self.compact {
                self.window_state.compact_size = clamp_size(bounds.size, MIN_COMPACT, MAX_COMPACT);
            } else {
                self.window_state.full_size = bounds.size;
            }
            self.window_state.origin = Some(bounds.origin);
        }
        if !self.compact {
            self.window_state.maximized = maximized;
        }
        self.pending_window_save = Some(Instant::now());
    }

    pub fn save_window_state(&mut self) {
        let mut state = self.library.setting("window_state").unwrap_or_else(|| json!({}));
        if !state.is_object() {
            state = json!({});
        }
        let s = &self.window_state;
        let object = state.as_object_mut().unwrap();
        object.insert("compact".into(), json!(self.compact));
        object.insert("full_size".into(), json!([f32::from(s.full_size.width) as i32, f32::from(s.full_size.height) as i32]));
        object.insert(
            "compact_size".into(),
            json!([f32::from(s.compact_size.width) as i32, f32::from(s.compact_size.height) as i32]),
        );
        if let Some(origin) = s.origin {
            object.insert("rs_origin".into(), json!([f32::from(origin.x) as i32, f32::from(origin.y) as i32]));
        }
        object.insert("rs_maximized".into(), json!(s.maximized));
        self.library.set_setting("window_state", state);
    }

    pub fn reset_window_layout(&mut self) {
        self.compact = false;
        self.window_state.full_size = Size { width: px(FULL_SIZE.0), height: px(FULL_SIZE.1) };
        self.window_state.compact_size = Size { width: px(COMPACT_SIZE.0), height: px(COMPACT_SIZE.1) };
        self.window_state.maximized = false;
        self.queue_open = false;
        self.library.set_setting("queue_open", json!(false));
        self.save_window_state();
        self.set_status(Message::new("window_layout_reset"));
    }

    pub fn tray_available(&self) -> bool {
        self.tray.is_some()
    }

    pub fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.save_window_state();
        self.quitting = true;
        self.sleep_timer.cancel();
        if let Some(job) = &self.scanner {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.playback.stop();
        self.cast.close();
        if let Some(mut session) = self.youtube.session.take() {
            session.close();
        }
        // Closes the headless browser, then the worker and its loopback server.
        self.spotify_player = None;
        self.spotify = None;
        self.tidal = None;
        if let Some(tray) = self.tray.take() {
            tray.shutdown();
        }
        cx.quit();
    }

    // ----- YouTube Music ---------------------------------------------------

    pub fn youtube_profile_dir(&self) -> PathBuf {
        self.library.path.parent().map(|p| p.join("ytmusic-profile")).unwrap_or_else(|| PathBuf::from("ytmusic-profile"))
    }

    pub fn account_status(&self) -> String {
        self.t(&format!("yt_auth_{}", self.youtube.auth_state()))
    }

    /// Start the helper process if needed. Returns `false` when it cannot run.
    pub fn ensure_youtube(&mut self) -> bool {
        if self.youtube.session.is_some() {
            return true;
        }
        let mut profile = Some(self.youtube_profile_dir());
        if self.youtube.forget_pending {
            match crate::youtube::session::forget_profile(profile.as_ref().unwrap()) {
                Ok(()) => {
                    self.youtube.forget_pending = false;
                    self.library.set_setting("yt_forget_pending", json!(false));
                }
                Err(_) => profile = None, // Never restore a session the user asked to remove.
            }
        }
        let dir = profile.unwrap_or_else(|| std::env::temp_dir().join(format!("fono8-ytmusic-{}", std::process::id())));
        match Session::spawn(&dir, &self.i18n, self.playback.engine.levels()) {
            Ok(session) => {
                self.playback.remote = Some(session.link());
                debug(|| format!("youtube: engine started ({:?})", session.kind));
                self.youtube.session = Some(session);
                true
            }
            Err(key) => {
                self.set_status(Message::new(key));
                false
            }
        }
    }

    pub fn youtube_show_account(&mut self) {
        if self.youtube.login.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let chromium =
                self.youtube.session.as_ref().map(|s| s.kind == crate::youtube::session::EngineKind::Chromium).unwrap_or(false)
                    || (self.youtube.session.is_none()
                        && crate::youtube::session::preferred_engine() == crate::youtube::session::EngineKind::Chromium);
            if chromium {
                self.chromium_login();
                return;
            }
        }
        if self.ensure_youtube() {
            if let Some(session) = &self.youtube.session {
                session.show(0);
            }
        }
    }

    /// Sign in through a plain browser window; the controlled session restarts when it closes.
    #[cfg(target_os = "linux")]
    fn chromium_login(&mut self) {
        let Some(browser) = crate::youtube::chromium::find_browser() else {
            self.set_status(Message::new("yt_browser_error"));
            return;
        };
        let profile = match self.youtube.session.take() {
            Some(mut session) => {
                self.playback.detach_remote();
                if self.youtube.operation.is_some() {
                    self.youtube.fail(Message::new("yt_cancelled"));
                }
                let profile = session.profile_dir.clone();
                session.close();
                profile
            }
            None => crate::youtube::chromium::profile_dir_for(&browser, &self.youtube_profile_dir()),
        };
        match crate::youtube::chromium::spawn_login(&browser, &profile) {
            Ok(child) => {
                self.youtube.login = Some(child);
                self.youtube.message = Message::new("yt_chromium_login");
            }
            Err(_) => self.set_status(Message::new("yt_browser_error")),
        }
    }

    #[cfg(target_os = "linux")]
    fn youtube_profile_dir_for_browser(&self) -> PathBuf {
        match crate::youtube::chromium::find_browser() {
            Some(browser) => crate::youtube::chromium::profile_dir_for(&browser, &self.youtube_profile_dir()),
            None => self.youtube_profile_dir(),
        }
    }

    /// "I'm signed in": close the plain browser window and resume the controlled session.
    pub fn youtube_finish_login(&mut self) {
        self.youtube.login_detected = None;
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.youtube.login.take() {
            crate::youtube::chromium::finish_login(&mut child);
            self.youtube.message = Message::new("yt_intro");
            if self.ensure_youtube() {
                if let Some(session) = &self.youtube.session {
                    session.refresh_auth();
                }
            }
        }
    }

    /// Called from the tick: resume the controlled session once the sign-in window is gone.
    /// A finished sign-in is detected from the profile and the window closed automatically.
    fn poll_youtube_login(&mut self) -> bool {
        if self.youtube.login.is_none() {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            if self.youtube.login_detected.is_none()
                && self.youtube.last_cookie_check.map(|t| now.duration_since(t) >= Duration::from_secs(1)).unwrap_or(true)
            {
                self.youtube.last_cookie_check = Some(now);
                let profile = self.youtube_profile_dir_for_browser();
                if crate::youtube::chromium::signed_in_cookie(&profile) {
                    debug(|| "youtube: sign-in detected in the profile".into());
                    self.youtube.login_detected = Some(now);
                    self.youtube.message = Message::new("yt_login_detected");
                    return true;
                }
            }
            if let Some(detected) = self.youtube.login_detected {
                if now.duration_since(detected) >= Duration::from_secs(1) {
                    self.youtube_finish_login();
                    return true;
                }
            }
        }
        let finished = matches!(self.youtube.login.as_mut().map(|child| child.try_wait()), Some(Ok(Some(_))));
        if !finished {
            return false;
        }
        self.youtube.login_detected = None;
        self.youtube.login = None;
        self.youtube.message = Message::new("yt_intro");
        if self.ensure_youtube() {
            if let Some(session) = &self.youtube.session {
                session.refresh_auth();
            }
        }
        true
    }

    fn youtube_start(&mut self, started: Result<(Operation, crate::youtube::provider::Step), Message>) {
        if self.youtube.busy {
            return;
        }
        match started {
            Ok((operation, step)) => {
                if !self.ensure_youtube() {
                    self.youtube.fail(Message::new("yt_browser_error"));
                    return;
                }
                if let Some(outcome) = self.youtube.start(operation, step) {
                    self.youtube_outcome(outcome);
                }
            }
            Err(message) => self.youtube.message = message,
        }
    }

    pub fn youtube_list_playlists(&mut self) {
        self.youtube_send(Request::Playlists);
    }

    pub fn youtube_import_address(&mut self, cx: &mut Context<Self>) {
        let address = self.discover_text(1, cx);
        self.youtube_send(Request::Import(address, false));
    }

    /// Start a YouTube Music request (kept for a retry once the account page is ready).
    fn youtube_send(&mut self, request: Request) {
        let started = match &request {
            Request::Search(query) => Operation::search(query),
            Request::Playlists => Ok(Operation::playlists()),
            Request::Import(address, enqueue) => Operation::import(address, *enqueue),
        };
        if started.is_ok() {
            self.youtube.retry = Some(request);
        }
        self.youtube_start(started);
    }

    /// "Add / import selected": tracks go to the target playlist, one playlist is imported.
    pub fn youtube_add_selection(&mut self) {
        if self.youtube.busy {
            return;
        }
        let items = self.youtube.selected_items();
        let Some(first) = items.first() else { return };
        match first {
            ResultItem::Playlist(playlist) => {
                if items.len() != 1 {
                    self.youtube.message = Message::new("yt_select_one");
                    return;
                }
                let id = playlist.id.clone();
                self.youtube_start(Operation::import(&id, false));
            }
            ResultItem::Track(_) => {
                let tracks: Vec<Track> =
                    items.into_iter().filter_map(|i| if let ResultItem::Track(t) = i { Some(t) } else { None }).collect();
                let target = self.youtube.target;
                match self.library.add_remote_tracks(&tracks, target) {
                    Ok(()) => {
                        self.refresh_playlists(None, true);
                        self.youtube.message = Message::new("tracks_added");
                    }
                    Err(_) => self.youtube.message = Message::new("yt_save_error"),
                }
            }
        }
    }

    pub fn youtube_enqueue_selection(&mut self) {
        if self.youtube.busy {
            return;
        }
        let items = self.youtube.selected_items();
        let Some(first) = items.first() else { return };
        match first {
            ResultItem::Playlist(playlist) => {
                if items.len() != 1 {
                    self.youtube.message = Message::new("yt_select_one");
                    return;
                }
                let id = playlist.id.clone();
                self.youtube_send(Request::Import(id, true));
            }
            ResultItem::Track(_) => {
                let tracks: Vec<Track> =
                    items.into_iter().filter_map(|i| if let ResultItem::Track(t) = i { Some(t) } else { None }).collect();
                self.youtube_enqueue_tracks(tracks);
            }
        }
    }

    fn youtube_enqueue_tracks(&mut self, tracks: Vec<Track>) {
        match self.library.add_remote_tracks(&tracks, None) {
            Ok(()) => {
                let count = tracks.len();
                self.refresh_playlists(None, true);
                self.enqueue_paths(tracks.into_iter().map(|t| t.path).collect());
                self.youtube.message = Message::new("queue_added").with("n", count);
            }
            Err(_) => self.youtube.message = Message::new("yt_save_error"),
        }
    }

    fn youtube_outcome(&mut self, outcome: Outcome) {
        if self.transfer_youtube(&outcome) {
            return;
        }
        match outcome {
            Outcome::Tracks(tracks) => {
                self.discover.clear_service(Service::YouTube);
                self.youtube.set_results(tracks.into_iter().map(ResultItem::Track).collect());
            }
            Outcome::Playlists(playlists) => {
                self.discover.clear_service(Service::YouTube);
                self.youtube.set_results(playlists.into_iter().map(ResultItem::Playlist).collect());
            }
            Outcome::Imported { name, tracks, enqueue } => {
                if enqueue {
                    self.youtube_enqueue_tracks(tracks);
                } else {
                    match self.library.import_remote_playlist(&name, &tracks) {
                        Ok(playlist) => {
                            self.refresh_playlists(Some(playlist), false);
                            self.youtube.message = Message::new("yt_imported").with("n", tracks.len());
                        }
                        Err(_) => self.youtube.message = Message::new("yt_save_error"),
                    }
                }
            }
            Outcome::Failed(message) => {
                // The account page is still loading after a (re)start: run the request again when it is ready.
                let transient = matches!(message.key, "yt_open_account" | "yt_cancelled" | "yt_busy");
                if transient && self.youtube.auth_state() == "checking" && self.youtube.retry.is_some() {
                    self.youtube.busy = true;
                    self.youtube.message = Message::new("yt_loading");
                    return;
                }
                self.youtube.retry = None;
                self.youtube.message = message;
            }
        }
        if !self.youtube.busy {
            self.youtube.retry = None;
        }
    }

    /// The account page became ready: rerun the request that failed while it was loading.
    fn youtube_retry(&mut self) {
        if self.youtube.operation.is_some() {
            return;
        }
        if let Some(request) = self.youtube.retry.take() {
            if self.youtube.busy {
                self.youtube.busy = false;
                self.youtube_send(request);
            }
        }
    }

    fn helper_event(&mut self, event: HelperEvent) {
        match &event {
            HelperEvent::Response { id, data, error } => {
                let known = self.youtube.session.as_mut().map(|s| s.pending.remove(id).is_some()).unwrap_or(false);
                if !known {
                    return;
                }
                let response = match (data, error) {
                    (Some(data), _) => Ok(data.clone()),
                    (None, Some(key)) => Err(Message::new(match key.as_str() {
                        "yt_auth_error" => "yt_auth_error",
                        "yt_open_account" => "yt_open_account",
                        "yt_cancelled" => "yt_cancelled",
                        "yt_busy" => "yt_busy",
                        _ => "yt_network_error",
                    })),
                    (None, None) => Err(Message::new("yt_network_error")),
                };
                if let Some(outcome) = self.youtube.respond(response) {
                    self.youtube_outcome(outcome);
                }
            }
            HelperEvent::Navigation => {
                // Navigation in the account tab cancels requests running there; they are retried once it is ready.
                if self.youtube.operation.is_some() {
                    if let Some(session) = self.youtube.session.as_mut() {
                        session.cancel();
                    }
                    self.youtube.operation = None;
                    if self.youtube.retry.is_some() {
                        self.youtube.busy = true;
                        self.youtube.message = Message::new("yt_loading");
                    } else {
                        self.youtube.fail(Message::new("yt_cancelled"));
                    }
                }
            }
            HelperEvent::Auth { state } => {
                if state != "checking" {
                    self.youtube_retry();
                }
            }
            HelperEvent::Warning { text } => {
                if !text.is_empty() {
                    self.youtube.message = Message::new("yt_navigation_blocked_text").with("text", text.clone());
                }
            }
            HelperEvent::Exited => {
                debug(|| "youtube: helper exited".into());
                self.playback.remote_event(&event);
                self.playback.detach_remote();
                self.youtube.session = None;
                if self.youtube.operation.is_some() {
                    self.youtube.fail(Message::new("yt_browser_error"));
                }
                self.set_status(Message::new("yt_browser_error"));
            }
            _ => self.playback.remote_event(&event),
        }
    }

    pub fn youtube_disconnect(&mut self) {
        if self.playback.current_remote() == Some(Remote::YouTube) {
            self.playback.stop();
            self.playback.queue.clear();
            self.playback.index = None;
            self.reset_now_playing();
        }
        self.playback.detach_remote();
        let Some(mut session) = self.youtube.session.take() else { return };
        self.youtube.fail(Message::new("yt_intro"));
        self.youtube.results.clear();
        self.youtube.selected.clear();
        self.library.set_setting("yt_forget_pending", json!(true));
        let profile = session.profile_dir.clone();
        session.close();
        let cleared = crate::youtube::session::forget_profile(&profile).is_ok();
        self.youtube.forget_pending = !cleared;
        self.library.set_setting("yt_forget_pending", json!(!cleared));
        self.set_status(Message::new(if cleared { "yt_disconnected" } else { "yt_forget_failed" }));
    }

    // ----- Spotify ---------------------------------------------------------

    pub fn spotify_profile_dir(&self) -> PathBuf {
        self.library.path.parent().map(|p| p.join("spotify-profile")).unwrap_or_else(|| PathBuf::from("spotify-profile"))
    }

    /// The Spotify worker, started on first use.
    pub fn spotify_client(&mut self) -> &Spotify {
        if self.spotify.is_none() {
            self.spotify = Some(Spotify::start(
                Box::new(crate::net::Https::new(crate::spotify::HOSTS)),
                Box::new(crate::keystore::Keyring { service: "spotify" }),
                crate::spotify::PORT,
                self.t("signed_in_page"),
            ));
        }
        self.spotify.as_ref().unwrap()
    }

    /// Start the browser sign-in with the user's Client ID (saved for next time).
    pub fn spotify_sign_in(&mut self, client_id: &str) {
        // A new sign-in replaces the current account: its player belongs to the old one.
        self.spotify_forget_account();
        let client_id = client_id.trim().to_ascii_lowercase();
        if crate::spotify::auth::is_client_id(&client_id) {
            self.library.set_setting("spotify_client_id", json!(client_id));
        }
        self.spotify_client().send(SpotifyRequest::SignIn { client_id });
    }

    pub fn spotify_sign_out(&mut self) {
        self.spotify_forget_account();
        if let Some(spotify) = &self.spotify {
            spotify.send(SpotifyRequest::SignOut);
        }
    }

    /// Stop Spotify playback and drop the player of the current account.
    fn spotify_forget_account(&mut self) {
        self.spotify_panel.clear();
        self.discover.clear_service(Service::Spotify);
        if self.playback.current_remote() == Some(Remote::Spotify) {
            self.playback.stop();
        }
        self.playback.detach_spotify();
        self.spotify_player = None;
        self.spotify_profile = None;
    }

    // ----- Discover and Settings -------------------------------------------

    /// Show the Discover page, optionally on one service's tab.
    pub fn open_discover(&mut self, filter: Option<Service>, cx: &mut Context<Self>) {
        self.menu = None;
        self.dialog = None;
        self.cast_open = false;
        if self.discover_inputs.is_none() {
            let query = self.new_input(cx, "discover_query");
            let address = self.new_input(cx, "discover_playlist_address");
            self.discover_subscriptions = vec![
                cx.subscribe(&query, |this, _, event, cx| {
                    if *event == InputEvent::Submitted {
                        this.discover_search(cx);
                    }
                    cx.notify();
                }),
                cx.subscribe(&address, |this, _, event, cx| {
                    if *event == InputEvent::Submitted {
                        if let Some(service) = this.discover.filter {
                            this.discover_import(service, cx);
                        }
                    }
                    cx.notify();
                }),
            ];
            self.discover_inputs = Some([query, address]);
        }
        if filter.is_some() {
            self.discover_filter(filter);
        }
        self.page = Page::Discover;
        self.query.clear();
    }

    pub fn open_settings(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.menu = None;
        self.dialog = None;
        self.cast_open = false;
        if self.spotify_client_input.is_none() {
            let input = self.new_input(cx, "spotify_client_id_hint");
            let saved = self.library.setting_string("spotify_client_id").unwrap_or_default();
            input.update(cx, |input, cx| input.set_text(saved, cx));
            self.discover_subscriptions.push(cx.subscribe(&input, |this, _, event, cx| {
                if *event == InputEvent::Submitted {
                    this.account_action(Service::Spotify, ServiceAction::Setup, cx);
                }
                cx.notify();
            }));
            self.spotify_client_input = Some(input);
        }
        if self.tidal_client_input.is_none() {
            let input = self.new_input(cx, "tidal_client_id_hint");
            let saved = self.library.setting_string("tidal_client_id").unwrap_or_default();
            input.update(cx, |input, cx| input.set_text(saved, cx));
            self.discover_subscriptions.push(cx.subscribe(&input, |this, _, event, cx| {
                if *event == InputEvent::Submitted {
                    this.account_action(Service::Tidal, ServiceAction::Setup, cx);
                }
                cx.notify();
            }));
            self.tidal_client_input = Some(input);
        }
        self.settings_tab = tab;
        self.page = Page::Settings;
        self.query.clear();
        if tab == SettingsTab::Accounts {
            if let Some(session) = &self.youtube.session {
                session.refresh_auth();
            }
        }
    }

    pub fn discover_filter(&mut self, filter: Option<Service>) {
        if self.discover.filter != filter {
            self.discover.filter = filter;
            self.discover.clear();
        }
    }

    fn discover_text(&self, index: usize, cx: &gpui::App) -> String {
        self.discover_inputs.as_ref().map(|inputs| inputs[index].read(cx).text().trim().to_string()).unwrap_or_default()
    }

    /// Search the selected service, or every usable one on the "All" tab.
    pub fn discover_search(&mut self, cx: &mut Context<Self>) {
        let query = self.discover_text(0, cx);
        if query.is_empty() {
            return;
        }
        let services: Vec<Service> = match self.discover.filter {
            Some(service) => vec![service],
            // Spotify needs an account; YouTube Music also searches signed out; TIDAL only imports.
            None => Service::ALL
                .into_iter()
                .filter(|s| s.searchable() && (*s != Service::Spotify || self.spotify_profile.is_some()))
                .collect(),
        };
        for service in services {
            match service {
                Service::YouTube => self.youtube_send(Request::Search(query.clone())),
                Service::Spotify => self.spotify_request(SpotifyRequest::Search { query: query.clone(), offset: 0 }),
                Service::Tidal => {}
            }
        }
    }

    pub fn discover_playlists(&mut self, service: Service) {
        match service {
            Service::YouTube => self.youtube_list_playlists(),
            Service::Spotify => {
                self.spotify_playlists_first = true;
                self.spotify_request(SpotifyRequest::Playlists { offset: 0 });
            }
            Service::Tidal => self.tidal_list_collections(),
        }
    }

    pub fn discover_liked(&mut self, service: Service) {
        if service == Service::Spotify {
            self.spotify_request(SpotifyRequest::Liked { enqueue: false });
        }
    }

    pub fn discover_import(&mut self, service: Service, cx: &mut Context<Self>) {
        match service {
            Service::YouTube => self.youtube_import_address(cx),
            Service::Spotify => {
                let playlist = self.discover_text(1, cx);
                self.spotify_request(SpotifyRequest::Import { playlist, enqueue: false });
            }
            Service::Tidal => {
                let link = self.discover_text(1, cx);
                self.tidal_open_link(link);
            }
        }
    }

    /// `true` when a visible service has another page of results.
    pub fn discover_has_more(&self) -> bool {
        self.spotify_panel.more.is_some() && self.discover.filter != Some(Service::YouTube)
    }

    pub fn discover_more(&mut self) {
        match self.spotify_panel.more.clone() {
            Some(SpotifyMore::Search { query, offset }) => self.spotify_request(SpotifyRequest::Search { query, offset }),
            Some(SpotifyMore::Playlists { offset }) => {
                self.spotify_playlists_first = false;
                self.spotify_request(SpotifyRequest::Playlists { offset });
            }
            None => {}
        }
    }

    fn results_len(&self, service: Service) -> usize {
        match service {
            Service::YouTube => self.youtube.results.len(),
            Service::Spotify => self.spotify_panel.results.len(),
            Service::Tidal => self.tidal_rows.len(),
        }
    }

    /// The rows on screen: one service, or all of them mixed by rank.
    pub fn discover_refs(&self) -> Vec<RowRef> {
        match self.discover.filter {
            Some(service) => (0..self.results_len(service)).map(|i| (service, i)).collect(),
            None => interleave(&Service::ALL.map(|s| (s, if s.searchable() { self.results_len(s) } else { 0 }))),
        }
    }

    pub fn discover_rows(&self) -> Vec<Row> {
        let t = |key: &str| self.t(key);
        self.discover_refs()
            .into_iter()
            .filter_map(|(service, index)| {
                let selected = self.discover.is_selected((service, index));
                let row = |title: String, subtitle: String, cover: Option<String>, enabled: bool| Row {
                    service,
                    title,
                    subtitle,
                    artist: None,
                    cover,
                    selected,
                    enabled,
                };
                let track_row = |track: &Track| Row {
                    artist: Some(track.artist.clone()),
                    ..row(track.title.clone(), self.i18n.artist(&track.artist), Some(track.path.clone()), true)
                };
                Some(match service {
                    Service::YouTube => match self.youtube.results.get(index)? {
                        ResultItem::Track(track) => track_row(track),
                        ResultItem::Playlist(playlist) => row(playlist.title.clone(), t("playlist"), None, true),
                    },
                    Service::Spotify => match self.spotify_panel.results.get(index)? {
                        SpotifyItem::Track(track) => track_row(track),
                        SpotifyItem::Liked => row(t("spotify_liked"), t("spotify_liked_hint"), None, true),
                        SpotifyItem::Playlist(playlist) => {
                            let mut subtitle = self.text(
                                "spotify_playlist_details",
                                &[("owner", Arg::Text(playlist.owner.clone())), ("n", Arg::Count(playlist.tracks as i64))],
                            );
                            if !playlist.importable {
                                subtitle = format!("{subtitle} · {}", t("spotify_followed_only"));
                            }
                            row(playlist.name.clone(), subtitle, None, playlist.importable)
                        }
                    },
                    Service::Tidal => self.tidal_row(self.tidal_rows.get(index)?, selected),
                })
            })
            .collect()
    }

    pub fn discover_select(&mut self, position: usize, control: bool, shift: bool) {
        let refs = self.discover_refs();
        self.discover.select(&refs, position, control, shift);
    }

    /// Double click: add just that row.
    pub fn discover_activate(&mut self, position: usize) {
        self.discover_select(position, false, false);
        self.discover_add(false);
    }

    /// Add (or enqueue) the selection; each service handles its own rows.
    pub fn discover_add(&mut self, enqueue: bool) {
        for service in self.discover.services_selected() {
            let indices: HashSet<usize> = self.discover.selected_for(service).into_iter().collect();
            match service {
                Service::YouTube => {
                    self.youtube.selected = indices;
                    self.youtube.target = self.discover.target;
                    if enqueue {
                        self.youtube_enqueue_selection();
                    } else {
                        self.youtube_add_selection();
                    }
                }
                Service::Spotify => {
                    self.spotify_panel.selected = indices;
                    self.spotify_panel.target = self.discover.target;
                    self.spotify_selection(enqueue);
                }
                Service::Tidal => self.tidal_add(indices, enqueue),
            }
        }
    }

    /// The service can be used on Discover (signed in where an account is needed).
    pub fn service_ready(&self, service: Service) -> bool {
        match service {
            Service::YouTube => true,
            Service::Spotify => self.spotify_profile.is_some(),
            Service::Tidal => self.tidal_profile.is_some(),
        }
    }

    pub fn discover_busy(&self) -> bool {
        self.youtube.busy || self.spotify_panel.busy || self.tidal_busy || self.transfer.is_some()
    }

    /// Status lines of the services shown on the current tab.
    pub fn discover_status(&self) -> Vec<(Service, String)> {
        let services: Vec<Service> = match self.discover.filter {
            Some(service) => vec![service],
            None => Service::ALL.into_iter().filter(|s| s.searchable()).collect(),
        };
        services
            .into_iter()
            .map(|service| {
                let text = match service {
                    Service::YouTube => self.i18n.message(&self.youtube.message),
                    Service::Spotify if self.spotify_profile.is_none() => self.t("spotify_discover_signed_out"),
                    Service::Spotify => self.i18n.message(&self.spotify_panel.message),
                    Service::Tidal if self.tidal_profile.is_none() => self.t("tidal_discover_signed_out"),
                    Service::Tidal => self.i18n.message(&self.tidal_message),
                };
                (service, text)
            })
            .collect()
    }

    pub fn target_name(&self, target: Option<i64>) -> String {
        target
            .and_then(|id| self.playlists.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| self.t("all_tracks"))
    }

    /// The card of `service` in Settings → Accounts.
    pub fn account_view(&self, service: Service) -> AccountView {
        let t = |key: &str| self.t(key);
        match service {
            Service::Tidal => self.tidal_account_view(),
            Service::YouTube => {
                let yt = &self.youtube;
                let session = if yt.persistent() { t("yt_session_short") } else { t("yt_session_temporary") };
                let account = if yt.login.is_some() {
                    Button::new(ServiceAction::Account, "check", t("yt_login_done")).accent(true)
                } else {
                    Button::new(ServiceAction::Account, "youtube", t("yt_account"))
                };
                AccountView {
                    service,
                    lines: vec![self.account_status(), session],
                    buttons: vec![
                        account,
                        Button::new(ServiceAction::Disconnect, "trash", t("yt_disconnect")).enabled(yt.session.is_some()),
                    ],
                    setup: None,
                    status: if yt.login.is_some() { t("yt_chromium_login") } else { String::new() },
                }
            }
            Service::Spotify => match &self.spotify_profile {
                Some(profile) => {
                    let plan = match profile.premium {
                        Some(false) => t("spotify_premium_required"),
                        _ => t("spotify_premium"),
                    };
                    AccountView {
                        service,
                        lines: vec![self.text("spotify_signed_in_as", &[("name", Arg::Text(profile.name.clone()))]), plan],
                        buttons: vec![
                            Button::new(ServiceAction::Account, "refresh", t("spotify_change_account")),
                            Button::new(ServiceAction::Disconnect, "trash", t("spotify_sign_out")),
                        ],
                        setup: None,
                        status: self.i18n.message(&self.spotify_account_message),
                    }
                }
                None => AccountView {
                    service,
                    lines: vec![t("spotify_status_signed_out"), t("spotify_notice")],
                    buttons: Vec::new(),
                    setup: self.spotify_client_input.clone().map(|input| {
                        (
                            input,
                            Button::new(ServiceAction::Setup, "spotify", t("spotify_sign_in")).accent(true),
                            self.text("spotify_redirect_hint", &[("uri", Arg::Text(crate::spotify::REDIRECT_URI.into()))]),
                        )
                    }),
                    status: self.i18n.message(&self.spotify_account_message),
                },
            },
        }
    }

    /// A button on an account card.
    pub fn account_action(&mut self, service: Service, action: ServiceAction, cx: &mut Context<Self>) {
        match (service, action) {
            (Service::Tidal, action) => self.tidal_account_action(action, cx),
            (Service::YouTube, ServiceAction::Account) => {
                if self.youtube.login.is_some() {
                    self.youtube_finish_login();
                } else {
                    self.youtube_show_account();
                }
            }
            (Service::YouTube, ServiceAction::Disconnect) => {
                self.discover.clear_service(Service::YouTube);
                self.youtube_disconnect();
            }
            (Service::YouTube, ServiceAction::Setup) => {}
            (Service::Spotify, ServiceAction::Setup) => {
                let client_id =
                    self.spotify_client_input.as_ref().map(|i| i.read(cx).text().trim().to_string()).unwrap_or_default();
                self.spotify_account_message = Message::new("spotify_login_waiting");
                self.spotify_sign_in(&client_id);
            }
            (Service::Spotify, ServiceAction::Account) => {
                let client_id = self.library.setting_string("spotify_client_id").unwrap_or_default();
                self.spotify_account_message = Message::new("spotify_login_waiting");
                self.spotify_sign_in(&client_id);
            }
            (Service::Spotify, ServiceAction::Disconnect) => self.spotify_sign_out(),
        }
    }

    fn spotify_request(&mut self, request: SpotifyRequest) {
        if self.spotify_profile.is_none() {
            self.spotify_panel.message = Message::new("spotify_login_required");
            return;
        }
        self.spotify_panel.busy = true;
        self.spotify_panel.message = Message::new("spotify_loading");
        self.spotify_client().send(request);
    }

    fn spotify_selection(&mut self, enqueue: bool) {
        if self.spotify_panel.busy {
            return;
        }
        let items = self.spotify_panel.selected_items();
        let tracks: Vec<Track> =
            items.iter().filter_map(|i| if let SpotifyItem::Track(t) = i { Some(t.clone()) } else { None }).collect();
        if tracks.is_empty() {
            match items.as_slice() {
                [SpotifyItem::Liked] => self.spotify_request(SpotifyRequest::Liked { enqueue }),
                [SpotifyItem::Playlist(playlist)] if playlist.importable => {
                    self.spotify_request(SpotifyRequest::Import { playlist: playlist.id.clone(), enqueue })
                }
                [SpotifyItem::Playlist(_)] => self.spotify_panel.message = Message::new("spotify_playlist_not_owned"),
                [] => {}
                _ => self.spotify_panel.message = Message::new("yt_select_one"),
            }
            return;
        }
        let target = if enqueue { None } else { self.spotify_panel.target };
        match self.library.add_remote_tracks(&tracks, target) {
            Ok(()) => {
                self.refresh_playlists(None, true);
                if enqueue {
                    let count = tracks.len();
                    self.enqueue_paths(tracks.into_iter().map(|t| t.path).collect());
                    self.spotify_panel.message = Message::new("queue_added").with("n", count);
                } else {
                    self.spotify_panel.message = Message::new("tracks_added");
                }
            }
            Err(_) => self.spotify_panel.message = Message::new("yt_save_error"),
        }
    }

    fn spotify_imported(&mut self, name: String, tracks: Vec<Track>, enqueue: bool) {
        self.spotify_panel.busy = false;
        if enqueue {
            match self.library.add_remote_tracks(&tracks, None) {
                Ok(()) => {
                    let count = tracks.len();
                    self.refresh_playlists(None, true);
                    self.enqueue_paths(tracks.into_iter().map(|t| t.path).collect());
                    self.spotify_panel.message = Message::new("queue_added").with("n", count);
                }
                Err(_) => self.spotify_panel.message = Message::new("yt_save_error"),
            }
            return;
        }
        let name = if name.is_empty() { self.t("spotify_liked") } else { name };
        match self.library.import_remote_playlist(&name, &tracks) {
            Ok(playlist) => {
                self.refresh_playlists(Some(playlist), false);
                self.spotify_panel.message = Message::new("yt_imported").with("n", tracks.len());
            }
            Err(_) => self.spotify_panel.message = Message::new("yt_save_error"),
        }
    }

    fn poll_spotify(&mut self, cx: &mut Context<Self>) -> bool {
        let updates = self.spotify.as_ref().map(Spotify::poll).unwrap_or_default();
        let mut changed = !updates.is_empty();
        for update in updates {
            match update {
                SpotifyUpdate::OpenUrl(url) => cx.open_url(&url),
                SpotifyUpdate::Status(message) => {
                    debug(|| format!("spotify: {}", message.key));
                    self.spotify_panel.message = message.clone();
                    self.spotify_account_message = message.clone();
                    self.set_status(message);
                }
                SpotifyUpdate::Failed(message) if self.transfer_spotify(None, &[]) => {
                    debug(|| format!("spotify: {} (import search)", message.key));
                }
                SpotifyUpdate::Results { query, page, .. } if self.transfer_spotify(Some(&query), &page.items) => {}
                SpotifyUpdate::Failed(message) => {
                    debug(|| format!("spotify: {}", message.key));
                    self.spotify_panel.busy = false;
                    self.spotify_panel.message = message.clone();
                    if message.key.starts_with("spotify_login")
                        || matches!(
                            message.key,
                            "spotify_auth_failed" | "spotify_client_invalid" | "spotify_port_busy" | "spotify_session_expired"
                        )
                    {
                        self.spotify_account_message = message.clone();
                    }
                    self.set_status(message);
                }
                SpotifyUpdate::SignedIn(profile) => {
                    debug(|| format!("spotify: signed in, premium {:?}", profile.premium));
                    if self.spotify_player.is_none() {
                        if let Some(spotify) = &self.spotify {
                            let player = SpotifyPlayer::start(spotify.link(), self.spotify_profile_dir());
                            self.playback.spotify = Some(player.link());
                            self.playback.set_volume(self.playback.volume);
                            self.playback.set_muted(self.playback.muted);
                            self.spotify_player = Some(player);
                        }
                    }
                    self.spotify_panel.message = Message::new("spotify_intro");
                    self.spotify_account_message = Message::new("spotify_signed_in_as").with("name", profile.name.clone());
                    self.spotify_profile = Some(profile);
                }
                SpotifyUpdate::SignedOut => {
                    self.playback.detach_spotify();
                    self.spotify_player = None;
                    self.spotify_profile = None;
                    self.spotify_panel.clear();
                    self.discover.clear_service(Service::Spotify);
                }
                SpotifyUpdate::Results { query, offset, page } => {
                    if offset == 0 {
                        self.discover.clear_service(Service::Spotify);
                    }
                    self.spotify_panel.search_results(query, offset, page);
                }
                SpotifyUpdate::Playlists(page) => {
                    let first = std::mem::replace(&mut self.spotify_playlists_first, true);
                    if first {
                        self.discover.clear_service(Service::Spotify);
                    }
                    self.spotify_panel.playlists(page, first);
                }
                SpotifyUpdate::Imported { name, tracks, enqueue } => self.spotify_imported(name, tracks, enqueue),
                SpotifyUpdate::Token(_) | SpotifyUpdate::PlayerUrl(_) | SpotifyUpdate::Playing => {}
            }
        }
        let events = self.spotify_player.as_ref().map(SpotifyPlayer::poll).unwrap_or_default();
        changed |= !events.is_empty();
        for event in events {
            self.playback.spotify_event(&event);
        }
        changed
    }

    // ----- Google Cast ---------------------------------------------------

    pub fn cast_status(&self) -> CastStatus {
        self.cast.status()
    }

    pub fn toggle_cast_panel(&mut self) {
        self.cast_open = !self.cast_open;
        self.menu = None;
        if self.cast_open {
            self.dialog = None;
            self.cast.discover();
        }
    }

    pub fn close_cast_panel(&mut self) {
        self.cast_open = false;
    }

    pub fn cast_refresh(&mut self) {
        self.cast.discover();
    }

    pub fn cast_to(&mut self, device: Option<String>) {
        self.cast.select(device);
    }

    pub fn cast_volume(&mut self, value: u32) {
        self.cast.set_volume(value);
    }

    pub fn cast_toggle_mute(&mut self) {
        self.cast.toggle_mute();
    }

    // ----- library views ------------------------------------------------

    pub fn refresh_playlists(&mut self, select: Option<i64>, preserve_view: bool) {
        let playlists = self.library.playlists();
        let valid: HashSet<i64> = playlists.iter().map(|p| p.id).collect();
        if let Some(select) = select {
            self.current_playlist = if valid.contains(&select) { Some(select) } else { None };
        } else if let Some(current) = self.current_playlist {
            if !valid.contains(&current) {
                self.current_playlist = None;
            }
        }
        let pinned = self.library.setting_ids("pinned_playlists");
        let mut rows: Vec<PlaylistRow> = playlists
            .iter()
            .map(|p| PlaylistRow {
                id: p.id,
                name: p.name.clone(),
                folder: p.folder.clone(),
                count: p.count,
                cover_path: self.library.first_path(p.id),
                pinned: pinned.contains(&p.id),
                favorite: Some(p.id) == self.favorites,
            })
            .map(|mut row| {
                if row.favorite {
                    row.name = self.t("favorites");
                }
                row
            })
            .collect();
        rows.sort_by_key(|row| (!row.favorite, !row.pinned));
        self.favorite_paths = match self.favorites {
            Some(id) => self.library.tracks(Some(id)).into_iter().map(|t| t.path).collect(),
            None => HashSet::new(),
        };
        self.playlists = rows;
        if !preserve_view {
            self.page = Page::Library;
            self.query.clear();
            self.selected.clear();
        }
        self.refresh_tracks();
        self.refresh_recent();
    }

    /// Find or create the "My favorites" playlist (its id is kept in the settings).
    fn ensure_favorites(&mut self) {
        let saved = self.library.setting("favorites_playlist").and_then(|v| v.as_i64());
        let exists = |id: i64| self.library.playlists().iter().any(|p| p.id == id);
        self.favorites = match saved.filter(|id| exists(*id)) {
            Some(id) => Some(id),
            None => {
                let name = self.t("favorites");
                let created = self.library.create_playlist(&name, None, &[]).ok();
                if let Some(id) = created {
                    self.library.set_setting("favorites_playlist", json!(id));
                }
                created
            }
        };
    }

    pub fn is_favorite(&self, path: &str) -> bool {
        self.favorite_paths.contains(path)
    }

    fn is_favorites_playlist(&self, id: i64) -> bool {
        self.favorites == Some(id)
    }

    /// Add the tracks to "My favorites", or remove them when all of them are already there.
    pub fn toggle_favorites(&mut self, paths: Vec<String>) {
        let Some(favorites) = self.favorites else { return };
        let paths: Vec<String> = paths.into_iter().filter(|p| self.library.track(p).is_some()).collect();
        if paths.is_empty() {
            return;
        }
        if paths.iter().all(|p| self.is_favorite(p)) {
            let _ = self.library.remove(favorites, &paths);
            self.set_status(Message::new("favorites_removed"));
        } else {
            let missing: Vec<String> = paths.into_iter().filter(|p| !self.is_favorite(p)).collect();
            let _ = self.library.add(favorites, &missing);
            self.set_status(Message::new("favorites_added"));
        }
        self.refresh_playlists(None, true);
    }

    /// "Search artist in Discover" for the first artist of one track.
    fn search_artist_item(&self, path: &str) -> Option<MenuEntry> {
        let track = self.library.track(path)?;
        let (artist, _) = library::artist_parts(&track.artist).into_iter().next()?;
        if self.i18n.artist(&track.artist) != track.artist {
            return None;
        }
        Some(item(self.text("artist_search_menu", &[("name", Arg::Text(artist.clone()))]), MenuAction::SearchOnline(artist)))
    }

    fn favorites_item(&self, paths: Vec<String>) -> MenuEntry {
        let all = !paths.is_empty() && paths.iter().all(|p| self.is_favorite(p));
        item(self.t(if all { "favorites_remove" } else { "favorites_add" }), MenuAction::ToggleFavorites(paths))
    }

    /// "Add to playlist" for the given tracks (every playlist except `except`).
    fn add_to_playlist_menu(&self, paths: &[String], except: Option<i64>) -> MenuEntry {
        let targets: Vec<MenuEntry> = self
            .playlists
            .iter()
            .filter(|p| Some(p.id) != except)
            .map(|p| item(p.name.clone(), MenuAction::AddPathsTo(paths.to_vec(), p.id)))
            .collect();
        MenuEntry::Submenu { label: self.t("add_to_playlist"), enabled: !targets.is_empty(), items: targets }
    }

    pub fn select_playlist(&mut self, playlist: Option<i64>) {
        if let Some(id) = playlist {
            if !self.playlists.iter().any(|p| p.id == id) {
                return;
            }
        }
        self.current_playlist = playlist;
        self.artist_filter = None;
        self.selected.clear();
        self.focused_path = None;
        self.page = Page::Library;
        self.query.clear();
        self.refresh_tracks();
    }

    /// All tracks of one artist, from every source in the library.
    pub fn show_artist(&mut self, artist: &str) {
        let artist = artist.trim();
        if artist.is_empty() {
            return;
        }
        self.menu = None;
        self.current_playlist = None;
        self.artist_filter = Some(artist.to_string());
        self.selected.clear();
        self.focused_path = None;
        self.page = Page::Library;
        self.query.clear();
        self.refresh_tracks();
    }

    /// Search the current Discover tab for one artist (a click on an artist in the results).
    pub fn discover_search_artist(&mut self, artist: &str, cx: &mut Context<Self>) {
        if let Some(inputs) = &self.discover_inputs {
            inputs[0].update(cx, |input, cx| input.set_text(artist.to_string(), cx));
        }
        self.discover_search(cx);
    }

    /// Search every connected service for `text` on the Discover page.
    pub fn search_online(&mut self, text: &str, cx: &mut Context<Self>) {
        self.open_discover(None, cx);
        self.discover_filter(None);
        if let Some(inputs) = &self.discover_inputs {
            inputs[0].update(cx, |input, cx| input.set_text(text.to_string(), cx));
        }
        self.discover_search(cx);
    }

    pub fn show_home(&mut self) {
        self.page = Page::Home;
        self.query.clear();
        self.refresh_recent();
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.page = Page::Library;
        self.selected.clear();
        self.refresh_tracks();
    }

    pub fn refresh_tracks(&mut self) {
        let query = self.query.trim().to_lowercase();
        let shown: Vec<Track> = self
            .library
            .tracks(self.current_playlist)
            .into_iter()
            .filter(|track| self.artist_filter.as_deref().is_none_or(|artist| library::has_artist(&track.artist, artist)))
            .filter(|track| {
                query.is_empty()
                    || format!("{} {} {}", track.title, self.i18n.artist(&track.artist), track.album)
                        .to_lowercase()
                        .contains(&query)
            })
            .collect();
        let paths: HashSet<&String> = shown.iter().map(|t| &t.path).collect();
        self.selected.retain(|p| paths.contains(p));
        self.tracks = shown;
    }

    pub fn current_playlist_row(&self) -> Option<&PlaylistRow> {
        self.current_playlist.and_then(|id| self.playlists.iter().find(|p| p.id == id))
    }

    pub fn heading(&self) -> String {
        match (&self.artist_filter, self.current_playlist_row()) {
            (Some(artist), _) => artist.clone(),
            (None, Some(playlist)) => playlist.name.clone(),
            (None, None) => self.t("all_tracks"),
        }
    }

    pub fn details(&self) -> String {
        let total: f64 = self.tracks.iter().map(|t| t.duration).sum();
        self.text(
            "track_summary",
            &[("tracks", Message::count("tracks_count", self.tracks.len()).into()), ("duration", format_duration(total).into())],
        )
    }

    pub fn empty_texts(&self) -> (String, String) {
        let key = if !self.query.trim().is_empty() {
            "no_results"
        } else if self.current_playlist_row().is_some() {
            "empty_playlist"
        } else {
            "empty_library"
        };
        (self.t(&format!("{key}_title")), self.t(&format!("{key}_body")))
    }

    pub fn can_reorder(&self) -> bool {
        self.current_playlist_row().is_some() && self.query.trim().is_empty()
    }

    pub fn visible_paths(&self) -> Vec<String> {
        self.tracks.iter().map(|t| t.path.clone()).collect()
    }

    pub fn selected_paths(&self) -> Vec<String> {
        self.tracks.iter().filter(|t| self.selected.contains(&t.path)).map(|t| t.path.clone()).collect()
    }

    pub fn select_track(&mut self, row: usize, control: bool, shift: bool) {
        let paths = self.visible_paths();
        if row >= paths.len() {
            return;
        }
        if shift {
            let anchor = self.selection_anchor.min(paths.len() - 1);
            let (start, end) = (anchor.min(row), anchor.max(row));
            let selection: HashSet<String> = paths[start..=end].iter().cloned().collect();
            if control {
                self.selected.extend(selection);
            } else {
                self.selected = selection;
            }
        } else if control {
            if !self.selected.remove(&paths[row]) {
                self.selected.insert(paths[row].clone());
            }
            self.selection_anchor = row;
        } else {
            self.selected = HashSet::from([paths[row].clone()]);
            self.selection_anchor = row;
        }
        self.focused_path = Some(paths[row].clone());
    }

    pub fn select_all(&mut self) {
        self.selected = self.visible_paths().into_iter().collect();
    }

    pub fn focused_row(&self) -> Option<usize> {
        self.focused_path.as_ref().and_then(|path| self.tracks.iter().position(|t| &t.path == path))
    }

    pub fn move_focus(&mut self, delta: i64, control: bool, shift: bool) {
        if self.tracks.is_empty() {
            return;
        }
        let current = self.focused_row().map(|r| r as i64).unwrap_or(-1);
        let next = (current + delta).clamp(0, self.tracks.len() as i64 - 1) as usize;
        self.select_track(next, control, shift);
    }

    pub fn reorder_track(&mut self, source: usize, target: usize) {
        let mut paths = self.visible_paths();
        if !self.can_reorder() || source >= paths.len() || target >= paths.len() {
            return;
        }
        let moved = paths.remove(source);
        paths.insert(target, moved);
        if let Some(playlist) = self.current_playlist {
            let _ = self.library.reorder(playlist, &paths);
        }
        self.refresh_tracks();
        self.set_status(Message::new("order_saved"));
    }

    pub fn move_selected(&mut self, direction: i64) {
        if !self.can_reorder() {
            return;
        }
        let mut paths = self.visible_paths();
        let indices: Vec<usize> = if direction < 0 { (0..paths.len()).collect() } else { (0..paths.len()).rev().collect() };
        for i in indices {
            let target = i as i64 + direction;
            if target < 0 || target >= paths.len() as i64 {
                continue;
            }
            let target = target as usize;
            if self.selected.contains(&paths[i]) && !self.selected.contains(&paths[target]) {
                paths.swap(i, target);
            }
        }
        if let Some(playlist) = self.current_playlist {
            let _ = self.library.reorder(playlist, &paths);
        }
        self.refresh_tracks();
        self.set_status(Message::new("order_saved"));
    }

    // ----- playback commands --------------------------------------------

    fn prepare_play(&mut self, paths: &[String]) {
        if let Some(first) = paths.first() {
            if let Some(track) = self.library.track(first) {
                self.playback.set_duration_hint(track.duration);
            }
        }
    }

    pub fn play_paths(&mut self, paths: Vec<String>) {
        if paths.first().map(|p| is_youtube_path(p)).unwrap_or(false) {
            self.ensure_youtube();
        }
        self.prepare_play(&paths);
        self.playback.prepend_and_play(paths);
    }

    pub fn start_selected(&mut self) {
        let paths = self.visible_paths();
        if paths.is_empty() {
            return;
        }
        let selected = match &self.focused_path {
            Some(path) if paths.contains(path) => path.clone(),
            _ => paths[0].clone(),
        };
        self.play_paths(vec![selected]);
    }

    pub fn start_visible(&mut self) {
        let paths = self.visible_paths();
        if !paths.is_empty() {
            self.play_paths(paths);
        }
    }

    pub fn play_track(&mut self, row: usize) {
        if let Some(track) = self.tracks.get(row) {
            let path = track.path.clone();
            self.play_paths(vec![path]);
        }
    }

    pub fn play_playlist(&mut self, playlist: i64) {
        if self.playlists.iter().any(|p| p.id == playlist) {
            self.select_playlist(Some(playlist));
            self.start_visible();
        }
    }

    pub fn play_recent(&mut self, path: &str) {
        if self.library.track(path).is_some() {
            self.play_paths(vec![path.to_string()]);
        }
    }

    pub fn play_queue(&mut self, index: usize) {
        if let Some(path) = self.playback.queue.get(index).cloned() {
            if is_youtube_path(&path) {
                self.ensure_youtube();
            }
            if let Some(track) = self.library.track(&path) {
                self.playback.set_duration_hint(track.duration);
            }
        }
        self.playback.play_index(index);
    }

    pub fn toggle_play(&mut self) {
        if self.playback.current().is_some() {
            if self.playback.current_remote() == Some(Remote::YouTube) {
                self.ensure_youtube();
            }
            self.playback.toggle();
        } else if !self.playback.queue.is_empty() {
            self.play_queue(0);
        } else {
            self.start_selected();
        }
    }

    pub fn next(&mut self) {
        self.ensure_youtube_for_queue();
        self.playback.next(false);
    }

    pub fn previous(&mut self) {
        self.ensure_youtube_for_queue();
        self.playback.previous();
    }

    fn ensure_youtube_for_queue(&mut self) {
        if self.playback.remote.is_none() && self.playback.queue.iter().any(|p| is_youtube_path(p)) {
            self.ensure_youtube();
        }
    }

    pub fn seek(&mut self, ms: u64) {
        self.playback.seek(ms);
    }

    pub fn set_volume(&mut self, value: u32) {
        let value = value.min(100);
        self.playback.set_volume(value as f64 / 100.0);
        self.library.set_setting("volume", json!(value as f64 / 100.0));
    }

    pub fn volume(&self) -> u32 {
        (self.playback.volume * 100.0).round() as u32
    }

    pub fn toggle_mute(&mut self) {
        let muted = !self.playback.muted;
        self.playback.set_muted(muted);
    }

    pub fn toggle_shuffle(&mut self) {
        self.playback.shuffle = !self.playback.shuffle;
        self.library.set_setting("shuffle", json!(self.playback.shuffle));
    }

    pub fn toggle_repeat(&mut self) {
        self.playback.repeat = !self.playback.repeat;
        self.library.set_setting("repeat", json!(self.playback.repeat));
    }

    pub fn toggle_queue(&mut self) {
        self.queue_open = !self.queue_open;
        self.library.set_setting("queue_open", json!(self.queue_open));
    }

    pub fn toggle_pin(&mut self) {
        let Some(current) = self.current_playlist else { return };
        let mut pinned = self.library.setting_ids("pinned_playlists");
        if let Some(index) = pinned.iter().position(|id| *id == current) {
            pinned.remove(index);
        } else {
            pinned.push(current);
        }
        self.library.set_setting("pinned_playlists", json!(pinned));
        self.refresh_playlists(None, true);
    }

    pub fn set_translucent(&mut self, enabled: bool) {
        self.translucent = enabled;
        self.library.set_setting("translucent", json!(enabled));
    }

    pub fn toggle_compact(&mut self) {
        // `maximized` describes the full window; mini mode keeps it for the way back.
        self.compact = !self.compact;
        self.menu = None;
        self.save_window_state();
    }

    pub fn enqueue_paths(&mut self, paths: Vec<String>) {
        let paths: Vec<String> = paths.into_iter().filter(|p| self.library.track(p).is_some()).collect();
        if paths.is_empty() {
            return;
        }
        let added = self.playback.enqueue(paths);
        self.queue_open = true;
        self.library.set_setting("queue_open", json!(true));
        if added > 0 {
            self.set_status(Message::new("queue_added").with("n", added));
        } else {
            self.set_status(Message::new("queue_already_present"));
        }
    }

    pub fn enqueue_selected(&mut self) {
        self.enqueue_paths(self.selected_paths());
    }

    pub fn enqueue_playlist(&mut self, playlist: Option<i64>) {
        let paths = if playlist.is_none() && self.artist_filter.is_some() {
            self.visible_paths()
        } else {
            self.library.tracks(playlist).into_iter().map(|t| t.path).collect()
        };
        self.enqueue_paths(paths);
    }

    pub fn clear_queue(&mut self) {
        self.playback.clear_queue();
        self.reset_now_playing();
        self.set_status(Message::new("queue_cleared"));
    }

    pub fn remove_from_queue(&mut self, index: usize) {
        if index >= self.playback.queue.len() {
            return;
        }
        self.playback.remove_from_queue(index);
        if self.playback.current().is_none() {
            self.reset_now_playing();
        }
        self.set_status(Message::new("queue_removed"));
    }

    pub fn reorder_queue(&mut self, source: usize, target: usize) {
        self.playback.reorder_queue(source, target);
    }

    pub fn queue_tracks(&self) -> Vec<(usize, Track)> {
        self.playback.queue.iter().enumerate().filter_map(|(i, path)| self.library.track(path).map(|t| (i, t))).collect()
    }

    fn reset_now_playing(&mut self) {
        self.current_track = None;
        self.update_tray_song();
    }

    /// The streaming service of the current track while audio goes to a Cast device.
    pub fn remote_beside_cast(&self) -> Option<Remote> {
        self.playback.current_remote().filter(|_| self.playback.casting)
    }

    fn track_changed(&mut self, path: &str) {
        let Some(track) = self.library.track(path) else { return };
        self.playback.set_duration_hint(track.duration);
        let status = match crate::playback::remote_kind(path) {
            Some(remote) if self.playback.casting => cast_soon_key(remote),
            Some(Remote::YouTube) => "yt_playing",
            Some(Remote::Spotify) => "spotify_playing",
            None if crate::tidal::is_track_path(path) => "tidal_playing_preview",
            None => "playing_library",
        };
        self.set_status(Message::new(status));
        self.current_track = Some(track);
        self.update_tray_song();
        let mut recent = self.library.setting_strings("recent_tracks");
        recent.retain(|p| p != path);
        recent.truncate(23);
        recent.insert(0, path.to_string());
        self.library.set_setting("recent_tracks", json!(recent));
        self.refresh_recent();
    }

    fn update_tray_song(&mut self) {
        let (song, tooltip) = match &self.current_track {
            Some(track) => (
                track.title.chars().take(70).collect::<String>(),
                format!("{} - {}", self.i18n.artist(&track.artist), track.title),
            ),
            None => (self.t("idle_tray"), self.t("tray_tooltip")),
        };
        if song != self.last_tray_song {
            self.last_tray_song = song.clone();
            if let Some(tray) = &self.tray {
                tray.set_song(song, tooltip);
            }
        }
    }

    pub fn window_title(&self) -> String {
        match &self.current_track {
            Some(track) => format!("{} - Fono8", track.title),
            None => "Fono8".into(),
        }
    }

    fn refresh_recent(&mut self) {
        let recent = self.library.setting_strings("recent_tracks");
        self.recent = recent.iter().filter_map(|path| self.library.track(path)).take(12).collect();
    }

    // ----- scanning -----------------------------------------------------

    pub fn scan(&mut self, folder: &str) {
        if self.scanner.is_some() {
            self.set_status(Message::new("scan_busy"));
            return;
        }
        self.busy = true;
        self.set_status(Message::new("scan_started"));
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let folder = folder.to_string();
        let cancel_flag = cancel.clone();
        let _ = std::thread::Builder::new().name("fono8-scan".into()).spawn(move || {
            let progress_sender = sender.clone();
            let result = library::scan_folder(Path::new(&folder), &cancel_flag, |count| {
                let _ = progress_sender.send(ScanMessage::Progress(count));
            });
            let message = match result {
                Ok(Some((tracks, errors))) => ScanMessage::Done(folder, tracks, errors),
                Ok(None) => ScanMessage::Cancelled,
                Err(message) => ScanMessage::Failed(message),
            };
            let _ = sender.send(message);
        });
        self.scanner = Some(ScanJob { cancel, receiver });
    }

    pub fn scanning(&self) -> bool {
        self.scanner.is_some()
    }

    fn scan_complete(&mut self, folder: &str, tracks: Vec<Track>, errors: Vec<String>, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if tracks.is_empty() {
            let warning: Arg = if errors.is_empty() { "".into() } else { Message::new("scan_inaccessible").into() };
            self.set_status(Message::new("scan_no_tracks").with("message", Message::new("scan_empty")).with("warning", warning));
            return;
        }
        match self.library.import_scan(folder, &tracks) {
            Ok(playlist) => {
                self.library.set_setting("last_folder", json!(folder));
                self.refresh_playlists(Some(playlist), false);
                let warning: Arg = if errors.is_empty() { "".into() } else { Message::new("scan_skipped").into() };
                self.set_status(
                    Message::new("scan_done")
                        .with("tracks", Message::count("tracks_count", tracks.len()))
                        .with("warning", warning),
                );
                let paths: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
                self.review_folder_metadata(&paths, false, cx);
            }
            Err(error) => self.set_status(Message::new("scan_error").with("error", error.to_string())),
        }
    }

    // ----- metadata review ----------------------------------------------

    pub fn review_folder_metadata(&mut self, paths: &[String], edit: bool, cx: &mut Context<Self>) {
        let proposals = self.library.folder_metadata_proposals(paths, edit);
        debug(|| format!("metadata review: {} proposal(s) for {} path(s), edit={edit}", proposals.len(), paths.len()));
        if proposals.is_empty() {
            if edit {
                self.set_status(Message::new("metadata_nothing"));
            }
            return;
        }
        let artist = self.new_input(cx, "metadata_artist");
        let album = self.new_input(cx, "metadata_album");
        let subscriptions = vec![self.watch_input(&artist, cx), self.watch_input(&album, cx)];
        self.dialog = Some(Dialog::Metadata { proposals, index: 0, artist, album, error: false, _subscriptions: subscriptions });
        self.load_metadata_proposal(cx);
    }

    fn load_metadata_proposal(&mut self, cx: &mut Context<Self>) {
        if let Some(Dialog::Metadata { proposals, index, artist, album, error, .. }) = &mut self.dialog {
            let proposal = &proposals[*index];
            *error = false;
            for (field, input) in [("artist", artist.clone()), ("album", album.clone())] {
                let enabled = !proposal.targets_for(field).is_empty();
                let value = if enabled { proposal.value_for(field).to_string() } else { String::new() };
                input.update(cx, |input, cx| {
                    input.set_text(value, cx);
                    input.set_enabled(enabled, cx);
                });
            }
        }
    }

    pub fn metadata_apply(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::Metadata { proposals, index, artist, album, .. }) = &self.dialog else { return };
        let proposal = proposals[*index].clone();
        let artist = artist.read(cx).text().to_string();
        let album = album.read(cx).text().to_string();
        match self.library.confirm_folder_metadata(&proposal, &artist, &album) {
            Ok(()) => {
                self.refresh_playlists(None, true);
                if let Some(current) = self.playback.current().map(str::to_string) {
                    self.current_track = self.library.track(&current);
                }
                self.set_status(Message::new("metadata_saved"));
                self.metadata_advance(cx);
            }
            Err(_) => {
                if let Some(Dialog::Metadata { error, .. }) = &mut self.dialog {
                    *error = true;
                }
            }
        }
    }

    pub fn metadata_advance(&mut self, cx: &mut Context<Self>) {
        let mut close = false;
        if let Some(Dialog::Metadata { proposals, index, .. }) = &mut self.dialog {
            if *index + 1 >= proposals.len() {
                close = true;
            } else {
                *index += 1;
            }
        }
        if close {
            self.dialog = None;
        } else {
            self.load_metadata_proposal(cx);
        }
    }

    pub fn metadata_valid(&self, cx: &gpui::App) -> bool {
        let Some(Dialog::Metadata { proposals, index, artist, album, .. }) = &self.dialog else { return false };
        let proposal = &proposals[*index];
        [("artist", artist), ("album", album)]
            .iter()
            .all(|(field, input)| proposal.targets_for(field).is_empty() || !input.read(cx).text().trim().is_empty())
    }

    // ----- dialogs and menus --------------------------------------------

    fn new_input(&self, cx: &mut Context<Self>, placeholder_key: &str) -> Entity<TextInput> {
        let placeholder = self.t(placeholder_key);
        let font = self.font_family.clone();
        cx.new(|cx| TextInput::new(cx, placeholder, font))
    }

    fn watch_input(&self, input: &Entity<TextInput>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(input, |this, _input, event, cx| match event {
            InputEvent::Submitted => this.dialog_submit(cx),
            InputEvent::Cancelled => this.close_dialog(),
            InputEvent::Changed => cx.notify(),
        })
    }

    pub fn open_input_dialog(&mut self, kind: InputKind, initial: &str, cx: &mut Context<Self>) {
        let (title, label, note): (&'static str, &'static str, Option<&'static str>) = match kind {
            InputKind::NewPlaylist => ("new_playlist", "playlist_name", None),
            InputKind::RenamePlaylist(_) => ("rename_playlist", "playlist_name", None),
            InputKind::SaveQueue => {
                let paths = &self.playback.queue;
                let unique: HashSet<&String> = paths.iter().collect();
                (
                    "save_queue_playlist",
                    "playlist_name",
                    if unique.len() != paths.len() { Some("queue_playlist_duplicates") } else { None },
                )
            }
        };
        let input = self.new_input(cx, label);
        input.update(cx, |input, cx| {
            input.set_text(initial.to_string(), cx);
            input.select_all(cx);
        });
        let subscription = self.watch_input(&input, cx);
        self.menu = None;
        self.dialog = Some(Dialog::Input { kind, title, label, note, input, _subscription: subscription });
    }

    pub fn dialog_submit(&mut self, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::Input { kind, input, .. }) => {
                let name = input.read(cx).text().trim().to_string();
                let kind = *kind;
                if name.is_empty() {
                    return;
                }
                self.dialog = None;
                match kind {
                    InputKind::NewPlaylist => {
                        if let Ok(id) = self.library.create_playlist(&name, None, &[]) {
                            self.refresh_playlists(Some(id), false);
                        }
                    }
                    InputKind::RenamePlaylist(id) if !self.is_favorites_playlist(id) => {
                        let _ = self.library.rename(id, &name);
                        self.refresh_playlists(None, false);
                    }
                    InputKind::RenamePlaylist(_) => {}
                    InputKind::SaveQueue => {
                        let paths: Vec<String> =
                            self.playback.queue.iter().filter(|p| self.library.track(p).is_some()).cloned().collect();
                        if paths.is_empty() {
                            return;
                        }
                        match self.library.create_playlist(&name, None, &paths) {
                            Ok(_) => {
                                self.refresh_playlists(None, true);
                                self.set_status(Message::new("queue_playlist_saved").with("name", name));
                            }
                            Err(_) => {
                                self.dialog = Some(Dialog::Message {
                                    title: self.t("save_queue_playlist"),
                                    body: self.t("queue_playlist_failed"),
                                });
                            }
                        }
                    }
                }
            }
            Some(Dialog::Confirm { kind, .. }) => {
                let kind = *kind;
                self.dialog = None;
                match kind {
                    ConfirmKind::DeletePlaylist(id) if !self.is_favorites_playlist(id) => {
                        let _ = self.library.delete(id);
                        if self.current_playlist == Some(id) {
                            self.current_playlist = None;
                        }
                        self.refresh_playlists(None, false);
                    }
                    ConfirmKind::DeletePlaylist(_) => {}
                }
            }
            Some(Dialog::SleepTimer { .. }) => self.sleep_timer_start(cx),
            Some(Dialog::Metadata { .. }) => {
                if self.metadata_valid(cx) {
                    self.metadata_apply(cx);
                }
            }
            Some(Dialog::Message { .. }) => self.dialog = None,
            None => {}
        }
    }

    pub fn close_dialog(&mut self) {
        self.dialog = None;
    }

    pub fn new_playlist(&mut self, cx: &mut Context<Self>) {
        self.open_input_dialog(InputKind::NewPlaylist, "", cx);
    }

    pub fn rename_playlist(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.is_favorites_playlist(id) {
            self.set_status(Message::new("favorites_protected"));
            return;
        }
        let name = self.playlists.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default();
        self.open_input_dialog(InputKind::RenamePlaylist(id), &name, cx);
    }

    pub fn save_queue_playlist(&mut self, cx: &mut Context<Self>) {
        if self.playback.queue.iter().all(|p| self.library.track(p).is_none()) {
            return;
        }
        let default = self.t("queue_playlist_default");
        self.open_input_dialog(InputKind::SaveQueue, &default, cx);
    }

    pub fn confirm_delete_playlist(&mut self, id: i64) {
        self.menu = None;
        if self.is_favorites_playlist(id) {
            self.set_status(Message::new("favorites_protected"));
            return;
        }
        self.dialog = Some(Dialog::Confirm {
            kind: ConfirmKind::DeletePlaylist(id),
            title: "delete_playlist_title",
            body: "delete_playlist_body",
        });
    }

    pub fn open_sleep_timer(&mut self, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::SleepTimer { .. })) {
            self.dialog = None;
            return;
        }
        let hours = self.new_input(cx, "sleep_timer_hours");
        let minutes = self.new_input(cx, "sleep_timer_minutes");
        hours.update(cx, |input, cx| {
            input.set_text("0".into(), cx);
            input.numeric = true;
        });
        minutes.update(cx, |input, cx| {
            input.set_text("30".into(), cx);
            input.numeric = true;
        });
        let subscriptions = vec![self.watch_input(&hours, cx), self.watch_input(&minutes, cx)];
        self.menu = None;
        self.dialog = Some(Dialog::SleepTimer { hours, minutes, _subscriptions: subscriptions });
    }

    pub fn sleep_timer_minutes(&self, cx: &gpui::App) -> u64 {
        let Some(Dialog::SleepTimer { hours, minutes, .. }) = &self.dialog else { return 0 };
        let hours: u64 = hours.read(cx).text().trim().parse().unwrap_or(0);
        let minutes: u64 = minutes.read(cx).text().trim().parse().unwrap_or(0);
        hours.min(23) * 60 + minutes.min(59)
    }

    pub fn sleep_timer_preset(&mut self, minutes: u64, cx: &mut Context<Self>) {
        if let Some(Dialog::SleepTimer { hours, minutes: minutes_input, .. }) = &self.dialog {
            hours.update(cx, |input, cx| input.set_text((minutes / 60).to_string(), cx));
            minutes_input.update(cx, |input, cx| input.set_text((minutes % 60).to_string(), cx));
        }
    }

    pub fn sleep_timer_start(&mut self, cx: &mut Context<Self>) {
        let minutes = self.sleep_timer_minutes(cx);
        if minutes > 0 {
            self.sleep_timer.start(minutes);
            self.dialog = None;
        }
    }

    pub fn sleep_timer_cancel(&mut self) {
        self.sleep_timer.cancel();
        self.dialog = None;
    }

    pub fn open_menu(&mut self, position: Point<Pixels>, items: Vec<MenuEntry>) {
        self.menu = Some(ContextMenu { position, items, parent: None });
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    pub fn track_menu(&mut self, position: Point<Pixels>) {
        let selected = self.selected_paths();
        if selected.is_empty() {
            return;
        }
        let filtering = !self.query.trim().is_empty();
        let mut items =
            vec![item(self.t("play"), MenuAction::PlaySelected), item(self.t("add_to_queue"), MenuAction::EnqueueSelected)];
        items.push(self.favorites_item(selected.clone()));
        items.push(self.add_to_playlist_menu(&selected, self.current_playlist));
        if let [path] = selected.as_slice() {
            items.extend(self.search_artist_item(path));
        }
        if selected.iter().any(|p| !is_remote_path(p)) {
            items.push(item(self.t("metadata_edit"), MenuAction::EditMetadataSelected));
        }
        if self.current_playlist.is_some() {
            items.push(MenuEntry::Item {
                label: self.t("move_up"),
                action: MenuAction::MoveUp,
                enabled: !filtering,
                checked: None,
            });
            items.push(MenuEntry::Item {
                label: self.t("move_down"),
                action: MenuAction::MoveDown,
                enabled: !filtering,
                checked: None,
            });
            items.push(MenuEntry::Separator);
            items.push(item(self.t("remove_from_playlist"), MenuAction::RemoveSelected));
        }
        self.open_menu(position, items);
    }

    pub fn playlist_menu(&mut self, id: i64, position: Point<Pixels>) {
        let Some(playlist) = self.playlists.iter().find(|p| p.id == id).cloned() else { return };
        let mut items = vec![item(self.t("add_playlist_to_queue"), MenuAction::EnqueuePlaylist(id)), MenuEntry::Separator];
        if playlist.favorite {
            items.push(item(self.t("export_m3u"), MenuAction::ExportPlaylist(id)));
            self.open_menu(position, items);
            return;
        }
        items.push(item(self.t("rename_playlist"), MenuAction::RenamePlaylist(id)));
        items.push(item(self.t("export_m3u"), MenuAction::ExportPlaylist(id)));
        items.push(item(self.t("metadata_edit"), MenuAction::EditPlaylistMetadata(id)));
        if let Some(folder) = playlist.folder {
            items.push(MenuEntry::Item {
                label: self.t("rescan"),
                action: MenuAction::Rescan(folder),
                enabled: !self.scanning(),
                checked: None,
            });
        }
        items.push(MenuEntry::Separator);
        items.push(item(self.t("delete_playlist_action"), MenuAction::DeletePlaylist(id)));
        self.open_menu(position, items);
    }

    pub fn queue_menu(&mut self, index: usize, position: Point<Pixels>) {
        let Some(path) = self.playback.queue.get(index).cloned() else { return };
        let mut items = Vec::new();
        if self.library.track(&path).is_some() {
            items.push(self.favorites_item(vec![path.clone()]));
            items.push(self.add_to_playlist_menu(&[path.clone()], None));
            items.extend(self.search_artist_item(&path));
            items.push(MenuEntry::Separator);
        }
        items.push(item(self.t("remove_from_queue"), MenuAction::RemoveFromQueue(index)));
        self.open_menu(position, items);
    }

    pub fn recent_menu(&mut self, path: &str, position: Point<Pixels>) {
        if self.library.track(path).is_none() {
            return;
        }
        let items = vec![
            item(self.t("play"), MenuAction::PlayPath(path.to_string())),
            item(self.t("add_to_queue"), MenuAction::EnqueuePath(path.to_string())),
            self.favorites_item(vec![path.to_string()]),
            self.add_to_playlist_menu(&[path.to_string()], None),
        ];
        self.open_menu(position, items);
    }

    pub fn options_menu(&mut self, position: Point<Pixels>) {
        let items = vec![
            item(self.t("settings"), MenuAction::OpenSettings),
            item(self.t("mini_mode"), MenuAction::ToggleCompact),
            MenuEntry::Separator,
            item(self.t("quit_fono8"), MenuAction::Quit),
        ];
        self.open_menu(position, items);
    }

    pub fn open_submenu(&mut self, index: usize) {
        if let Some(menu) = &mut self.menu {
            if let Some(MenuEntry::Submenu { items, enabled: true, .. }) = menu.items.get(index).cloned() {
                let parent = std::mem::replace(&mut menu.items, items);
                menu.items.insert(0, item(self.i18n.t("menu_back"), MenuAction::Back));
                menu.items.insert(1, MenuEntry::Separator);
                menu.parent = Some(parent);
            }
        }
    }

    /// Run a menu action. Returns a request for the window when one is needed.
    pub fn menu_action(&mut self, action: MenuAction, cx: &mut Context<Self>) -> Option<WindowRequest> {
        if action != MenuAction::Back {
            self.menu = None;
        }
        match action {
            MenuAction::Back => {
                if let Some(menu) = &mut self.menu {
                    if let Some(parent) = menu.parent.take() {
                        menu.items = parent;
                    }
                }
            }
            MenuAction::PlaySelected => self.start_selected(),
            MenuAction::EnqueueSelected => self.enqueue_selected(),
            MenuAction::EditMetadataSelected => {
                let paths = self.selected_paths();
                self.review_folder_metadata(&paths, true, cx);
            }
            MenuAction::MoveUp => self.move_selected(-1),
            MenuAction::MoveDown => self.move_selected(1),
            MenuAction::RemoveSelected => self.remove_selected(),
            MenuAction::EnqueuePlaylist(id) => self.enqueue_playlist(Some(id)),
            MenuAction::RenamePlaylist(id) => self.rename_playlist(id, cx),
            MenuAction::ExportPlaylist(id) => return Some(WindowRequest::ExportPlaylist(id)),
            MenuAction::EditPlaylistMetadata(id) => {
                let paths: Vec<String> = self.library.tracks(Some(id)).into_iter().map(|t| t.path).collect();
                self.review_folder_metadata(&paths, true, cx);
            }
            MenuAction::Rescan(folder) => self.scan(&folder),
            MenuAction::DeletePlaylist(id) => self.confirm_delete_playlist(id),
            MenuAction::RemoveFromQueue(index) => self.remove_from_queue(index),
            MenuAction::PlayPath(path) => self.play_recent(&path),
            MenuAction::EnqueuePath(path) => self.enqueue_paths(vec![path]),
            MenuAction::ToggleTranslucent => {
                let enabled = !self.translucent;
                self.set_translucent(enabled);
                return Some(WindowRequest::ApplyTranslucency);
            }
            MenuAction::ToggleCompact => return Some(WindowRequest::ToggleCompact),
            MenuAction::ResetLayout => return Some(WindowRequest::ResetLayout),
            MenuAction::Language(code) => self.change_language(&code),
            MenuAction::DiscoverTarget(target) => self.discover.target = target,
            MenuAction::SearchOnline(text) => self.search_online(&text, cx),
            MenuAction::AddPathsTo(paths, playlist) => {
                let existing: HashSet<String> = self.library.tracks(Some(playlist)).into_iter().map(|t| t.path).collect();
                let missing: Vec<String> = paths.into_iter().filter(|p| !existing.contains(p)).collect();
                let _ = self.library.add(playlist, &missing);
                self.refresh_playlists(None, true);
                self.set_status(Message::new("tracks_added"));
            }
            MenuAction::ToggleFavorites(paths) => self.toggle_favorites(paths),
            MenuAction::OpenSettings => self.open_settings(SettingsTab::Accounts, cx),
            MenuAction::Quit => return Some(WindowRequest::Quit),
        }
        None
    }

    pub fn remove_selected(&mut self) {
        if let Some(playlist) = self.current_playlist {
            let _ = self.library.remove(playlist, &self.selected_paths());
            self.refresh_playlists(None, false);
            self.set_status(Message::new("tracks_removed"));
        }
    }

    pub fn export_playlist(&self, playlist: i64, destination: &Path) -> Result<(), String> {
        let mut lines = vec!["#EXTM3U".to_string()];
        for track in self.library.tracks(Some(playlist)) {
            let name = format!("{} - {}", self.i18n.artist(&track.artist), track.title).replace(['\n', '\r'], " ");
            if track.path.contains('\n') || track.path.contains('\r') {
                return Err(self.t("newline_filename"));
            }
            lines.push(format!("#EXTINF:{},{}", track.duration as i64, name));
            lines.push(track.external_path());
        }
        std::fs::write(destination, lines.join("\n") + "\n").map_err(|e| e.to_string())
    }

    pub fn last_folder(&self) -> PathBuf {
        self.library
            .setting_string("last_folder")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn show_message(&mut self, title: String, body: String) {
        self.dialog = Some(Dialog::Message { title, body });
    }

    // ----- drag and drop reordering --------------------------------------

    pub fn begin_drag(&mut self, list: DragList, source: usize, y: f32) {
        self.drag = Some(Drag { list, source, target: source, y });
    }

    pub fn update_drag(&mut self, target: usize, y: f32) {
        if let Some(drag) = &mut self.drag {
            drag.target = target;
            drag.y = y;
        }
    }

    pub fn cancel_drag(&mut self) {
        self.drag = None;
    }

    pub fn finish_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            if drag.source != drag.target {
                match drag.list {
                    DragList::Tracks => self.reorder_track(drag.source, drag.target),
                    DragList::Queue => self.reorder_queue(drag.source, drag.target),
                }
            }
        }
    }
}

/// Window-level follow-ups for actions decided in the model.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowRequest {
    ExportPlaylist(i64),
    ApplyTranslucency,
    ToggleCompact,
    ResetLayout,
    Quit,
}

fn item(label: String, action: MenuAction) -> MenuEntry {
    MenuEntry::Item { label, action, enabled: true, checked: None }
}

fn clamp_size(size: Size<Pixels>, min: (f32, f32), max: (f32, f32)) -> Size<Pixels> {
    Size { width: px(f32::from(size.width).clamp(min.0, max.0)), height: px(f32::from(size.height).clamp(min.1, max.1)) }
}

fn pick_font(cx: &gpui::App) -> String {
    let available = cx.text_system().all_font_names();
    for candidate in ["Inter", "Ubuntu", "Noto Sans", "Cantarell", "DejaVu Sans", "Liberation Sans", "Segoe UI", "Helvetica"] {
        if available.iter().any(|name| name == candidate) {
            return candidate.to_string();
        }
    }
    available.first().cloned().unwrap_or_else(|| "sans-serif".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_opens_at_origin_zero_on_wayland() {
        let saved = Some(Point { x: px(300.0), y: px(200.0) });
        assert_eq!(window_origin(saved, true), Point::default());
        assert_eq!(window_origin(None, true), Point::default());
        assert_eq!(window_origin(saved, false), Point { x: px(300.0), y: px(200.0) });
        assert_eq!(window_origin(None, false), Point { x: px(120.0), y: px(80.0) });
    }
}
