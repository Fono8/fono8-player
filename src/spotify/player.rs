//! Spotify playback through the official Web Playback SDK, hosted by an
//! installed Chrome, Edge or Brave (they ship Widevine) running headless and
//! driven over the DevTools pipe. The page is `assets/spotify/player.*`, served
//! by the loopback server; tokens and play requests go through the client worker.
//!
//! Fono8 owns the queue, so Spotify plays one track at a time: the engine
//! reports when that track ends (the SDK pauses at position 0) or when Spotify
//! moves on by itself, and pauses it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cdp::{self, Connection};
use crate::i18n::Message;

use super::client::{ClientLink, Request, Update};
use super::TRACK_PREFIX;

/// Browsers with Widevine first; distribution Chromium usually lacks it.
const BROWSERS: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "microsoft-edge-stable",
    "microsoft-edge",
    "brave-browser",
    "brave",
    "chromium",
    "chromium-browser",
];
#[cfg(target_os = "macos")]
const MAC_BROWSERS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
];
const START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(250);
/// How close to the end the last progress must be for "paused at 0" to mean "ended".
const END_SLACK_MS: u64 = 4000;

/// The SDK error codes from `player.js`.
fn sdk_error(code: &str) -> Message {
    Message::new(match code {
        "initialization_error" => "spotify_drm_error",
        "authentication_error" => "spotify_auth_failed",
        "account_error" => "spotify_premium_required",
        "autoplay_failed" => "spotify_autoplay_error",
        "sdk_load_error" => "spotify_sdk_failed",
        _ => "spotify_playback_error",
    })
}

/// `FONO8_BROWSER`, then the first browser with Widevine on `PATH` (or in /Applications on macOS).
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("FONO8_BROWSER") {
        let path = PathBuf::from(custom);
        return path.is_file().then_some(path);
    }
    #[cfg(target_os = "macos")]
    for candidate in MAC_BROWSERS {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    let path = std::env::var_os("PATH")?;
    BROWSERS.iter().find_map(|name| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|c| c.is_file()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    State {
        playing: bool,
        position_ms: u64,
        duration_ms: u64,
    },
    Ended,
    Error(Message),
    /// The browser went away; the next load starts it again.
    Exited,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Load { uri: String },
    Resume,
    Pause,
    Stop,
    Seek(u64),
    Volume(f32),
    Muted(bool),
    Quit,
}

/// Control handle for `Playback`; cheap to clone.
#[derive(Clone)]
pub struct SpotifyLink {
    commands: Sender<Command>,
}

impl SpotifyLink {
    pub fn load(&self, track_id: &str) -> bool {
        self.commands.send(Command::Load { uri: format!("{TRACK_PREFIX}{track_id}") }).is_ok()
    }

    pub fn play(&self) {
        let _ = self.commands.send(Command::Resume);
    }

    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    pub fn seek(&self, ms: u64) {
        let _ = self.commands.send(Command::Seek(ms));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.commands.send(Command::Volume(volume));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::Muted(muted));
    }
}

/// One SDK `player_state_changed` snapshot as reported by `player.js`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SdkState {
    pub available: bool,
    pub paused: bool,
    pub position: u64,
    pub duration: u64,
    pub uri: String,
    pub original_uri: String,
}

impl SdkState {
    fn from_event(event: &Value) -> SdkState {
        let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let number =
            |key: &str| event.get(key).and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0) as u64;
        SdkState {
            available: event.get("available").and_then(Value::as_bool).unwrap_or(false),
            paused: event.get("paused").and_then(Value::as_bool).unwrap_or(true),
            position: number("position"),
            duration: number("duration"),
            uri: text("uri"),
            original_uri: text("original_uri"),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Reaction {
    Emit(PlayerEvent),
    /// Spotify continued past Fono8's track: pause it.
    Pause,
}

/// Follows the SDK states of the one track Fono8 asked for.
#[derive(Default)]
pub struct Tracker {
    wanted: Option<String>,
    requested: Option<Instant>,
    confirmed: bool,
    /// The last progress seen while playing: position, duration and when.
    progress: Option<(u64, u64, Instant)>,
}

impl Tracker {
    pub fn request(&mut self, uri: &str, now: Instant) {
        self.wanted = Some(uri.to_string());
        self.requested = Some(now);
        self.confirmed = false;
        self.progress = None;
    }

    pub fn clear(&mut self) {
        *self = Tracker::default();
    }

    pub fn wanted(&self) -> Option<&str> {
        self.wanted.as_deref()
    }

    /// The play request got no matching state in time.
    pub fn timed_out(&mut self, now: Instant) -> bool {
        let late =
            self.wanted.is_some() && !self.confirmed && self.requested.is_some_and(|at| now.duration_since(at) >= START_TIMEOUT);
        if late {
            self.clear();
        }
        late
    }

    pub fn state(&mut self, state: &SdkState, now: Instant) -> Vec<Reaction> {
        let Some(wanted) = self.wanted.clone() else { return Vec::new() };
        if !state.available {
            return Vec::new();
        }
        let ours = state.uri == wanted || state.original_uri == wanted;
        if !ours {
            if self.confirmed {
                // Autoplay or another app moved Spotify on: our track is over.
                self.clear();
                return vec![Reaction::Pause, Reaction::Emit(PlayerEvent::Ended)];
            }
            return Vec::new(); // a late state of the previous track
        }
        if self.confirmed && state.paused && state.duration > 0 {
            let at_end = state.position + 1000 >= state.duration;
            let rewound = state.position == 0
                && self.progress.is_some_and(|(position, duration, at)| {
                    position + now.duration_since(at).as_millis() as u64 + END_SLACK_MS >= duration
                });
            if at_end || rewound {
                self.clear();
                return vec![Reaction::Emit(PlayerEvent::Ended)];
            }
        }
        self.confirmed = true;
        if !state.paused {
            self.progress = Some((state.position, state.duration, now));
        }
        vec![Reaction::Emit(PlayerEvent::State {
            playing: !state.paused,
            position_ms: state.position,
            duration_ms: state.duration,
        })]
    }
}

struct Browser {
    child: Child,
    connection: Connection,
    session: String,
    connected: bool,
    probed: bool,
}

struct Engine {
    client: ClientLink,
    profile: PathBuf,
    events: Sender<PlayerEvent>,
    browser: Option<Browser>,
    device: Option<String>,
    tracker: Tracker,
    /// A track to start once the SDK device is ready.
    pending: Option<String>,
    volume: f32,
    muted: bool,
    debug: bool,
}

impl Engine {
    fn log(&self, message: impl FnOnce() -> String) {
        if self.debug {
            eprintln!("[fono8 spotify] {}", message());
        }
    }

    fn emit(&self, event: PlayerEvent) {
        let _ = self.events.send(event);
    }

    fn fail(&mut self, message: Message) {
        self.log(|| format!("error: {}", message.key));
        self.tracker.clear();
        self.pending = None;
        self.emit(PlayerEvent::Error(message));
    }

    fn script(&mut self, expression: &str) -> Option<Value> {
        let browser = self.browser.as_mut()?;
        let session = browser.session.clone();
        browser.connection.evaluate(&session, expression)
    }

    fn command(&mut self, name: &str, value: Option<Value>) {
        let value = value.map(|v| v.to_string()).unwrap_or_else(|| "undefined".into());
        let _ = self.script(&format!("window.fono8Spotify && window.fono8Spotify.command({}, {value})", json!(name)));
    }

    fn apply_volume(&mut self) {
        let volume = if self.muted { 0.0 } else { self.volume.clamp(0.0, 1.0) };
        self.command("volume", Some(json!(volume)));
    }

    fn ensure_browser(&mut self) -> bool {
        if self.browser.as_ref().is_some_and(|b| !b.connection.closed()) {
            return true;
        }
        self.shutdown_browser();
        let Some(path) = find_browser() else {
            self.fail(Message::new("spotify_browser_missing"));
            return false;
        };
        let url = self.client.call(Request::PlayerUrl, Duration::from_secs(10)).into_iter().find_map(|update| match update {
            Update::PlayerUrl(url) => Some(Ok(url)),
            Update::Failed(message) => Some(Err(message)),
            _ => None,
        });
        let url = match url {
            Some(Ok(url)) => url,
            Some(Err(message)) => {
                self.fail(message);
                return false;
            }
            None => {
                self.fail(Message::new("spotify_sdk_failed"));
                return false;
            }
        };
        match launch(&path, &self.profile, &url, self.debug) {
            Ok(browser) => {
                self.log(|| format!("browser started: {}", path.display()));
                self.browser = Some(browser);
                true
            }
            Err(error) => {
                self.log(|| format!("browser launch failed: {error}"));
                self.fail(Message::new("spotify_sdk_failed"));
                false
            }
        }
    }

    fn shutdown_browser(&mut self) {
        if let Some(mut browser) = self.browser.take() {
            browser.connection.close_browser();
            for _ in 0..30 {
                if matches!(browser.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = browser.child.kill();
            let _ = browser.child.wait();
        }
        self.device = None;
    }

    fn start(&mut self, uri: String) {
        let Some(device) = self.device.clone() else {
            self.pending = Some(uri);
            return;
        };
        self.command("arm", None);
        self.tracker.request(&uri, Instant::now());
        let updates = self.client.call(Request::Play { device, uri, position_ms: 0 }, Duration::from_secs(20));
        if let Some(message) = updates.iter().find_map(|u| if let Update::Failed(m) = u { Some(m.clone()) } else { None }) {
            self.fail(message);
        } else if !updates.contains(&Update::Playing) {
            self.fail(Message::new("spotify_playback_error"));
        }
    }

    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Load { uri } => {
                self.tracker.clear();
                if self.ensure_browser() {
                    self.start(uri);
                }
            }
            Command::Resume => {
                if self.tracker.wanted().is_some() {
                    self.command("resume", None);
                }
            }
            Command::Pause => self.command("pause", None),
            Command::Stop => {
                self.pending = None;
                if self.tracker.wanted().is_some() {
                    self.command("pause", None);
                }
                self.tracker.clear();
            }
            Command::Seek(ms) => self.command("seek", Some(json!(ms))),
            Command::Volume(volume) => {
                self.volume = volume;
                self.apply_volume();
            }
            Command::Muted(muted) => {
                self.muted = muted;
                self.apply_volume();
            }
            Command::Quit => return false,
        }
        true
    }

    fn poll(&mut self) {
        let Some(browser) = self.browser.as_ref() else { return };
        if browser.connection.closed() {
            self.log(|| "browser exited".into());
            let playing = self.tracker.wanted().is_some();
            self.shutdown_browser();
            self.tracker.clear();
            if playing {
                self.emit(PlayerEvent::Exited);
            }
            return;
        }
        let ready = self.script("!!window.fono8Spotify").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ready {
            return;
        }
        if let Some(browser) = self.browser.as_mut() {
            if !browser.connected {
                browser.connected = true;
                let session = browser.session.clone();
                browser.connection.evaluate(&session, "window.fono8Spotify.connect()");
            }
        }
        if self.browser.as_ref().is_some_and(|b| !b.probed) {
            if let Some(browser) = self.browser.as_mut() {
                browser.probed = true;
            }
            let probe = "navigator.requestMediaKeySystemAccess('com.widevine.alpha', [{initDataTypes: ['cenc'], \
                         audioCapabilities: [{contentType: 'audio/mp4; codecs=\"mp4a.40.2\"', robustness: 'SW_SECURE_CRYPTO'}]}])\
                         .then(() => true).catch(() => false)";
            if self.script(probe).and_then(|v| v.as_bool()) == Some(false) {
                self.fail(Message::new("spotify_drm_error"));
            }
        }
        let events = self.script("window.fono8Spotify.drain()").and_then(|v| v.as_array().cloned()).unwrap_or_default();
        for event in events {
            match event.get("kind").and_then(Value::as_str).unwrap_or("") {
                "sdk_loaded" => {
                    let _ = self.script("window.fono8Spotify.connect()");
                }
                "token" => {
                    let token = self.client.call(Request::Token, Duration::from_secs(20)).into_iter().find_map(|u| match u {
                        Update::Token(token) => token,
                        _ => None,
                    });
                    let argument = token.map(|t| json!(t).to_string()).unwrap_or_else(|| "null".into());
                    let _ = self.script(&format!("window.fono8Spotify.provideToken({argument})"));
                }
                "ready" => {
                    self.device = event
                        .get("device_id")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty() && d.len() <= 200)
                        .map(str::to_string);
                    self.log(|| "device ready".into());
                    self.apply_volume();
                    if let Some(uri) = self.pending.take() {
                        self.start(uri);
                    }
                }
                "not_ready" => self.device = None,
                "state" => {
                    let state = SdkState::from_event(&event);
                    for reaction in self.tracker.state(&state, Instant::now()) {
                        match reaction {
                            Reaction::Pause => self.command("pause", None),
                            Reaction::Emit(event) => self.emit(event),
                        }
                    }
                }
                "error" => {
                    let code = event.get("code").and_then(Value::as_str).unwrap_or("").to_string();
                    let wanted = self.tracker.wanted().is_some() || self.pending.is_some();
                    self.log(|| format!("sdk error {code}"));
                    if wanted || code != "playback_error" {
                        self.fail(sdk_error(&code));
                    }
                }
                _ => {}
            }
        }
        if self.tracker.timed_out(Instant::now()) {
            self.fail(Message::new("spotify_start_timeout"));
        }
    }
}

fn launch(path: &Path, profile: &Path, url: &str, debug: bool) -> std::io::Result<Browser> {
    std::fs::create_dir_all(profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700));
    }
    let mut process = Process::new(path);
    process
        .arg("--headless=new")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-sync")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--autoplay-policy=no-user-gesture-required")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if debug { Stdio::inherit() } else { Stdio::null() });
    let (child, writer, reader) = cdp::launch(process)?;
    let replies = cdp::read_messages(reader, "fono8-spotify-cdp")?;
    let mut connection = Connection::new(writer, replies);
    let timeout = Duration::from_secs(15);
    let failure = || std::io::Error::other("DevTools setup failed");
    let target = connection.call(None, "Target.createTarget", json!({"url": "about:blank"}), timeout).ok_or_else(failure)?;
    let target_id = target.get("targetId").and_then(Value::as_str).ok_or_else(failure)?.to_string();
    let attached = connection
        .call(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), timeout)
        .ok_or_else(failure)?;
    let session = attached.get("sessionId").and_then(Value::as_str).ok_or_else(failure)?.to_string();
    connection.call(Some(&session), "Page.navigate", json!({"url": url}), timeout).ok_or_else(failure)?;
    Ok(Browser { child, connection, session, connected: false, probed: false })
}

/// A link whose commands are only recorded (tests of `Playback`).
#[cfg(test)]
pub(crate) fn test_link() -> (SpotifyLink, Receiver<Command>) {
    let (commands, received) = mpsc::channel();
    (SpotifyLink { commands }, received)
}

/// Owns the engine thread; dropping it closes the browser.
pub struct SpotifyPlayer {
    link: SpotifyLink,
    events: Receiver<PlayerEvent>,
    thread: Option<JoinHandle<()>>,
}

impl SpotifyPlayer {
    /// The browser starts lazily, on the first track.
    pub fn start(client: ClientLink, profile: PathBuf) -> SpotifyPlayer {
        let (commands, inbox) = mpsc::channel();
        let (events_sender, events) = mpsc::channel();
        let mut engine = Engine {
            client,
            profile,
            events: events_sender,
            browser: None,
            device: None,
            tracker: Tracker::default(),
            pending: None,
            volume: 0.65,
            muted: false,
            debug: std::env::var_os("FONO8_DEBUG").is_some(),
        };
        let thread = std::thread::Builder::new()
            .name("fono8-spotify-player".into())
            .spawn(move || {
                loop {
                    let wait = if engine.browser.is_some() { POLL } else { Duration::from_secs(3600) };
                    match inbox.recv_timeout(wait) {
                        Ok(command) => {
                            if !engine.handle(command) {
                                break;
                            }
                            // Apply queued commands (volume drags, rapid skips) before polling.
                            let mut quit = false;
                            while let Ok(command) = inbox.try_recv() {
                                if !engine.handle(command) {
                                    quit = true;
                                    break;
                                }
                            }
                            if quit {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    engine.poll();
                }
                let _ = engine.script("window.fono8Spotify && window.fono8Spotify.disconnect()");
                engine.shutdown_browser();
            })
            .ok();
        SpotifyPlayer { link: SpotifyLink { commands }, events, thread }
    }

    pub fn link(&self) -> SpotifyLink {
        self.link.clone()
    }

    pub fn poll(&self) -> Vec<PlayerEvent> {
        self.events.try_iter().collect()
    }

    /// Wait for the next event (tests).
    #[cfg(test)]
    pub fn wait(&self, timeout: Duration) -> Option<PlayerEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for SpotifyPlayer {
    fn drop(&mut self) {
        let _ = self.link.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "spotify:track:0DiWol3AO6WpXZgp0goxAV";

    fn state(paused: bool, position: u64, uri: &str) -> SdkState {
        SdkState { available: true, paused, position, duration: 320_000, uri: uri.into(), original_uri: String::new() }
    }

    fn emitted(reactions: Vec<Reaction>) -> Vec<PlayerEvent> {
        reactions.into_iter().filter_map(|r| if let Reaction::Emit(e) = r { Some(e) } else { None }).collect()
    }

    #[test]
    fn reports_progress_and_detects_the_end_by_rewind() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        assert!(tracker.state(&state(false, 10, URI), now).is_empty(), "nothing requested");
        tracker.request(URI, now);
        assert!(tracker.state(&state(true, 0, "spotify:track:0DiWol3AO6WpXZgp0goxAW"), now).is_empty(), "stale state");
        assert_eq!(
            emitted(tracker.state(&state(false, 0, URI), now)),
            [PlayerEvent::State { playing: true, position_ms: 0, duration_ms: 320_000 }]
        );
        let later = now + Duration::from_secs(300);
        tracker.state(&state(false, 310_000, URI), later);
        // A user pause mid-track is not an end.
        let paused = emitted(tracker.state(&state(true, 150_000, URI), later));
        assert_eq!(paused, [PlayerEvent::State { playing: false, position_ms: 150_000, duration_ms: 320_000 }]);
        tracker.state(&state(false, 310_000, URI), later);
        // The SDK reports the finished track as paused at 0 shortly after the last progress.
        assert_eq!(emitted(tracker.state(&state(true, 0, URI), later + Duration::from_secs(9))), [PlayerEvent::Ended]);
        assert!(tracker.wanted().is_none());
    }

    #[test]
    fn pause_at_start_is_not_an_end_but_autoplay_is() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.request(URI, now);
        tracker.state(&state(false, 0, URI), now);
        tracker.state(&state(false, 2_000, URI), now + Duration::from_secs(2));
        let events = emitted(tracker.state(&state(true, 0, URI), now + Duration::from_secs(3)));
        assert_eq!(events, [PlayerEvent::State { playing: false, position_ms: 0, duration_ms: 320_000 }]);
        assert_eq!(
            tracker.state(&state(false, 0, "spotify:track:0DiWol3AO6WpXZgp0goxAW"), now + Duration::from_secs(4)),
            [Reaction::Pause, Reaction::Emit(PlayerEvent::Ended)]
        );
    }

    #[test]
    fn end_position_relinked_tracks_and_start_timeout() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.request(URI, now);
        let relinked = SdkState { original_uri: URI.into(), ..state(false, 5, "spotify:track:0DiWol3AO6WpXZgp0goxAW") };
        assert_eq!(emitted(tracker.state(&relinked, now)).len(), 1);
        assert_eq!(emitted(tracker.state(&state(true, 319_500, URI), now)), [PlayerEvent::Ended]);

        tracker.request(URI, now);
        assert!(!tracker.timed_out(now + Duration::from_secs(5)));
        assert!(tracker.timed_out(now + START_TIMEOUT));
        assert!(tracker.wanted().is_none());
        assert_eq!(sdk_error("account_error"), Message::new("spotify_premium_required"));
        assert_eq!(sdk_error("whatever"), Message::new("spotify_playback_error"));
    }

    #[test]
    fn parses_sdk_state_events_defensively() {
        let event = json!({"kind": "state", "available": true, "paused": false, "position": 12.7, "duration": -5, "uri": URI});
        let parsed = SdkState::from_event(&event);
        assert_eq!((parsed.available, parsed.paused, parsed.position, parsed.duration), (true, false, 12, 0));
        assert_eq!(SdkState::from_event(&json!({})), SdkState { paused: true, ..Default::default() });
    }

    /// Plays a track through the real SDK: needs a saved session (run `spotify_live` first).
    /// `FONO8_SPOTIFY_CLIENT_ID=... cargo test spotify_player_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn spotify_player_live() {
        use super::super::client::Spotify;
        let client_id = std::env::var("FONO8_SPOTIFY_CLIENT_ID").expect("FONO8_SPOTIFY_CLIENT_ID");
        let spotify = Spotify::start(
            Box::new(crate::net::Https::new(super::super::HOSTS)),
            Box::new(crate::keystore::Keyring { service: "spotify" }),
            super::super::PORT,
            String::new(),
        );
        spotify.send(Request::Restore { client_id });
        match spotify.wait(Duration::from_secs(20)) {
            Some(Update::SignedIn(profile)) => eprintln!("signed in as {:?}", profile.name),
            other => panic!("no saved session: {other:?}"),
        }
        let profile = std::env::temp_dir().join(format!("fono8-spotify-test-{}", std::process::id()));
        let player = SpotifyPlayer::start(spotify.link(), profile.clone());
        let link = player.link();
        link.set_volume(0.5);
        assert!(link.load("0DiWol3AO6WpXZgp0goxAV"));
        let started = Instant::now();
        let mut last = None;
        while started.elapsed() < Duration::from_secs(40) {
            match player.wait(Duration::from_secs(25)) {
                Some(PlayerEvent::State { playing: true, position_ms, duration_ms }) => {
                    last = Some((position_ms, duration_ms));
                    if position_ms > 5_000 {
                        break;
                    }
                }
                Some(PlayerEvent::Error(message)) => panic!("player error {}", message.key),
                Some(other) => eprintln!("event {other:?}"),
                None => panic!("no state from the SDK"),
            }
        }
        let (position, duration) = last.expect("playing state");
        eprintln!("playing at {position} ms of {duration} ms after {:?}", started.elapsed());
        link.seek(duration.saturating_sub(6_000));
        let ended = Instant::now();
        loop {
            match player.wait(Duration::from_secs(30)) {
                Some(PlayerEvent::Ended) => break,
                Some(PlayerEvent::Error(message)) => panic!("player error {}", message.key),
                Some(_) => assert!(ended.elapsed() < Duration::from_secs(30), "no end"),
                None => panic!("no end event"),
            }
        }
        eprintln!("ended after seeking: {:?}", ended.elapsed());
        drop(player);
        let _ = std::fs::remove_dir_all(profile);
    }
}
