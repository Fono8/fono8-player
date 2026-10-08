//! Engine-independent YouTube Music session logic: the bounded API transport,
//! sign-in state polling and control of the site's own player.
//!
//! Hosts provide an [`Engine`] (web views driven by `wry`, or a Chromium browser
//! driven over the DevTools protocol) and forward page events to [`Core`]; the
//! core answers with [`protocol::Event`]s.

pub mod protocol;
pub mod scripts;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use protocol::{Command, Event, Texts};

pub const ORIGIN: &str = "https://music.youtube.com";
pub const NAVIGATION_HOSTS: &[&str] = &[
    "music.youtube.com",
    "accounts.google.com",
    // Regional account redirect observed in the Polish Google sign-in flow.
    "accounts.google.pl",
    // Google may show account suggestions here during the sign-in flow.
    "gds.google.com",
    "accounts.youtube.com",
    "consent.youtube.com",
    "consent.google.com",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    Account,
    Player,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageLoad {
    Started,
    Finished,
}

/// Why a script was evaluated; returned to the core with the result.
#[derive(Clone, Debug)]
pub enum Purpose {
    Auth { generation: u64 },
    RequestConfig { id: String, endpoint: String, body: Value },
    RequestPoll,
    PlayerState { generation: u64 },
    Levels,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolbarState {
    pub notice: String,
    pub tooltip: String,
    pub home: String,
    pub tabs: [String; 2],
    pub current: usize,
    pub warning: String,
}

/// What a host must provide. Script evaluation is asynchronous: the result comes
/// back through [`Core::script_reply`] with the same [`Purpose`].
pub trait Engine {
    fn eval(&mut self, view: View, script: &str, purpose: Purpose);
    fn run(&mut self, view: View, script: &str);
    fn load_url(&mut self, view: View, url: &str);
    fn show(&mut self, view: View);
    fn hide(&mut self);
    fn toolbar(&mut self, state: &ToolbarState);
}

pub fn navigation_error(url: &str) -> Option<&'static str> {
    if url == "about:blank" {
        return None;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Some(if url.contains("://") { "yt_nav_protocol" } else { "yt_nav_invalid" });
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Some("yt_nav_credentials");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
        _ => (authority, None),
    };
    if host.is_empty() {
        return Some("yt_nav_invalid");
    }
    if port.is_some_and(|p| p != "443") {
        return Some("yt_nav_port");
    }
    if !NAVIGATION_HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        return Some("yt_nav_host");
    }
    None
}

pub fn host_of(url: &str) -> String {
    url.strip_prefix("https://").unwrap_or("").split(['/', '?', '#', ':']).next().unwrap_or("").to_ascii_lowercase()
}

pub fn path_of(url: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or("");
    let after_host = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    after_host.split(['?', '#']).next().unwrap_or("/").to_string()
}

pub fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
}

/// Script results arrive as the JSON encoding of the script's value; our scripts
/// return JSON *strings*, so decode twice.
pub fn unwrap_json_string(raw: &str) -> Option<Value> {
    let outer: Value = serde_json::from_str(raw).ok()?;
    match outer {
        // Plain string results (such as the auth state) are kept as strings.
        Value::String(inner) => Some(serde_json::from_str(&inner).unwrap_or(Value::String(inner))),
        other => Some(other),
    }
}

pub fn is_video_id(value: &str) -> bool {
    value.len() == 11 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

struct Pending {
    deadline: Instant,
}

pub struct Core {
    texts: Texts,
    language: String,
    persistent: bool,
    current_tab: View,
    events: Vec<Event>,
    log: Option<Box<dyn Fn(&str)>>,
    // account page / transport
    account_url: String,
    account_ready: bool,
    pub auth_state: String,
    auth_generation: u64,
    auth_polling: Option<Instant>,
    pending: HashMap<String, Pending>,
    request_polling: bool,
    last_request_poll: Instant,
    last_auth_poll: Instant,
    navigation_message: Option<String>,
    // player
    target: Option<String>,
    player_generation: u64,
    player_state: &'static str,
    position: u64,
    duration: u64,
    volume: f32,
    muted: bool,
    started: bool,
    start_requested: bool,
    waiting_for_load: bool,
    want_play: bool,
    deadline: Instant,
    player_polling: bool,
    last_player_poll: Instant,
    levels_polling: bool,
    last_levels_poll: Instant,
    pending_seek: Option<u64>,
    seek_started: Instant,
    seek_deadline: Instant,
}

impl Core {
    pub fn new(persistent: bool, language: &str) -> Core {
        let now = Instant::now();
        let mut core = Core {
            texts: Texts::default(),
            language: language.to_string(),
            persistent,
            current_tab: View::Account,
            events: Vec::new(),
            log: None,
            account_url: String::new(),
            account_ready: false,
            auth_state: "checking".into(),
            auth_generation: 0,
            auth_polling: None,
            pending: HashMap::new(),
            request_polling: false,
            last_request_poll: now,
            last_auth_poll: now,
            navigation_message: None,
            target: None,
            player_generation: 0,
            player_state: "stopped",
            position: 0,
            duration: 0,
            volume: 0.65,
            muted: false,
            started: false,
            start_requested: false,
            waiting_for_load: false,
            want_play: true,
            deadline: now,
            player_polling: false,
            last_player_poll: now,
            levels_polling: false,
            last_levels_poll: now,
            pending_seek: None,
            seek_started: now,
            seek_deadline: now,
        };
        core.events.push(Event::Ready { persistent });
        core
    }

    /// Diagnostics sink (never page contents or tokens).
    pub fn set_logger(&mut self, log: impl Fn(&str) + 'static) {
        self.log = Some(Box::new(log));
    }

    fn debug(&self, message: impl FnOnce() -> String) {
        if let Some(log) = &self.log {
            log(&message());
        }
    }

    fn emit(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Load the account page; call once the engine views exist.
    pub fn start(&mut self, engine: &mut dyn Engine) {
        self.set_auth_state("checking", engine);
        engine.load_url(View::Account, ORIGIN);
    }

    pub fn toolbar_state(&self) -> ToolbarState {
        ToolbarState {
            notice: format!(
                "{}\n{}",
                self.texts.auth.get(self.auth_state.as_str()).cloned().unwrap_or_default(),
                if self.persistent { &self.texts.session_short } else { &self.texts.session_temporary }
            ),
            tooltip: self.texts.session_notice.clone(),
            home: self.texts.home.clone(),
            tabs: [self.texts.account_tab.clone(), self.texts.player_tab.clone()],
            current: if self.current_tab == View::Account { 0 } else { 1 },
            warning: self.navigation_message.clone().unwrap_or_default(),
        }
    }

    fn update_toolbar(&mut self, engine: &mut dyn Engine) {
        let state = self.toolbar_state();
        engine.toolbar(&state);
    }

    fn set_auth_state(&mut self, state: &str, engine: &mut dyn Engine) {
        if self.auth_state != state {
            self.auth_state = state.to_string();
            self.emit(Event::Auth { state: state.to_string() });
        }
        self.update_toolbar(engine);
    }

    fn select_tab(&mut self, view: View, engine: &mut dyn Engine) {
        self.current_tab = view;
        engine.show(view);
        self.update_toolbar(engine);
    }

    // ----- host entry points ------------------------------------------------

    /// Toolbar messages: `tab:0`, `tab:1`, `home`.
    pub fn ipc(&mut self, message: &str, engine: &mut dyn Engine) {
        match message {
            "tab:0" => self.select_tab(View::Account, engine),
            "tab:1" => self.select_tab(View::Player, engine),
            "home" => {
                self.select_tab(View::Account, engine);
                engine.load_url(View::Account, ORIGIN);
            }
            _ => {}
        }
    }

    /// The engine's window was closed by the user.
    pub fn closed(&mut self) {
        self.emit(Event::Closed);
    }

    pub fn command(&mut self, command: Command, engine: &mut dyn Engine) {
        match command {
            Command::Init { texts, language } => {
                self.texts = texts;
                self.language = language;
                self.update_toolbar(engine);
            }
            Command::Show { tab } => self.select_tab(if tab == 1 { View::Player } else { View::Account }, engine),
            Command::Hide => engine.hide(),
            Command::Request { id, endpoint, body } => self.request(id, endpoint, body, engine),
            Command::Cancel => self.cancel_requests(engine),
            Command::RefreshAuth => self.refresh_auth(engine),
            Command::Load { video } => self.load(video, engine),
            Command::Play => self.play(engine),
            Command::Pause => self.pause(engine),
            Command::Stop => self.stop_player(engine),
            Command::Seek { ms } => self.seek(ms, engine),
            Command::Volume { value } => {
                self.volume = value.clamp(0.0, 1.0);
                let command = format!("p.setVolume({});", self.volume * 100.0);
                self.player_command(&command, engine);
            }
            Command::Muted { muted } => {
                self.muted = muted;
                self.apply_mute(engine);
            }
            Command::Quit => self.stop_player(engine),
        }
    }

    pub fn navigation_blocked(&mut self, view: View, host: &str, reason: &str, engine: &mut dyn Engine) {
        self.debug(|| format!("navigation blocked {view:?} host={host} reason={reason}"));
        if view != View::Account {
            return;
        }
        let reason_text = self.texts.navigation_reasons.get(reason).cloned().unwrap_or_else(|| reason.to_string());
        let message = self
            .texts
            .navigation_blocked
            .replace("{host}", if host.is_empty() { "?" } else { host })
            .replace("{reason}", &reason_text);
        self.navigation_message = Some(message.clone());
        self.emit(Event::Warning { text: message });
        self.update_toolbar(engine);
    }

    pub fn page_load(&mut self, view: View, event: PageLoad, url: &str, engine: &mut dyn Engine) {
        self.debug(|| format!("page load {view:?} {event:?} host={} path={}", host_of(url), path_of(url)));
        match (view, event) {
            (View::Account, PageLoad::Started) => {
                self.account_url = url.to_string();
                self.auth_generation += 1;
                self.auth_polling = None;
                self.account_ready = false;
                self.set_auth_state("checking", engine);
                self.cancel_requests(engine);
                self.emit(Event::Navigation);
                if navigation_error(url).is_none() && self.navigation_message.take().is_some() {
                    self.emit(Event::Warning { text: String::new() });
                    self.update_toolbar(engine);
                }
            }
            (View::Account, PageLoad::Finished) => {
                self.account_url = url.to_string();
                self.account_ready = true;
                self.refresh_auth(engine);
            }
            (View::Player, PageLoad::Started) => {
                if self.target.is_some() {
                    self.waiting_for_load = true;
                    self.start_requested = false;
                    self.player_generation += 1;
                    self.player_polling = false;
                }
                self.player_url_changed(url, engine);
            }
            (View::Player, PageLoad::Finished) => {
                if let Some(target) = &self.target {
                    if host_of(url) == "music.youtube.com"
                        && path_of(url) == "/watch"
                        && query_param(url, "v").as_deref() == Some(target)
                    {
                        self.waiting_for_load = false;
                    }
                }
            }
        }
    }

    /// Same-document navigation (history API), as the site does between songs.
    pub fn url_changed(&mut self, view: View, url: &str, engine: &mut dyn Engine) {
        if view == View::Player {
            self.player_url_changed(url, engine);
        } else if navigation_error(url).is_none() && self.navigation_message.take().is_some() {
            self.emit(Event::Warning { text: String::new() });
            self.update_toolbar(engine);
        }
    }

    pub fn script_reply(&mut self, purpose: Purpose, result: &str, engine: &mut dyn Engine) {
        match purpose {
            Purpose::Auth { generation } => {
                self.debug(|| format!("auth reply: {}", result.chars().take(60).collect::<String>()));
                if generation != self.auth_generation {
                    return;
                }
                self.auth_polling = None;
                let state = unwrap_json_string(result).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                let state = if state == "signed_in" || state == "signed_out" { state } else { "unknown".to_string() };
                self.set_auth_state(&state, engine);
            }
            Purpose::RequestConfig { id, endpoint, body } => self.configured(id, endpoint, body, result, engine),
            Purpose::RequestPoll => {
                self.request_polling = false;
                match unwrap_json_string(result) {
                    Some(Value::Object(completed)) => {
                        for (id, result) in completed {
                            if let Some(error) = result.get("error").and_then(Value::as_str) {
                                let error = error.to_string();
                                self.finish(&id, Err(&error));
                            } else if let Some(data) = result.get("data") {
                                if data.is_object() && data.get("error").is_none() {
                                    self.finish(&id, Ok(data.clone()));
                                } else {
                                    self.finish(&id, Err("yt_response_error"));
                                }
                            } else {
                                self.finish(&id, Err("yt_network_error"));
                            }
                        }
                    }
                    Some(_) => {}
                    None => self.cancel_requests(engine),
                }
            }
            Purpose::PlayerState { generation } => self.player_state_reply(generation, result, engine),
            Purpose::Levels => {
                self.levels_polling = false;
                let bands = unwrap_json_string(result)
                    .and_then(|v| v.as_array().cloned())
                    .map(|values| values.iter().filter_map(Value::as_f64).map(|v| v as f32).collect::<Vec<_>>());
                if let Some(bands) = bands.filter(|b| b.len() == 5 && self.player_state == "playing") {
                    self.emit(Event::Levels { bands });
                }
            }
        }
    }

    pub fn tick(&mut self, engine: &mut dyn Engine) {
        if self.last_request_poll.elapsed() >= Duration::from_millis(100) {
            self.last_request_poll = Instant::now();
            self.poll_requests(engine);
        }
        if self.last_auth_poll.elapsed() >= Duration::from_secs(2) {
            self.last_auth_poll = Instant::now();
            self.refresh_auth(engine);
        }
        if self.last_player_poll.elapsed() >= Duration::from_millis(250) {
            self.last_player_poll = Instant::now();
            self.poll_player(engine);
        }
        // Hosts tick every 40 ms; a reply still on its way skips a beat.
        if self.player_state == "playing" && self.target.is_some() && !self.levels_polling {
            self.last_levels_poll = Instant::now();
            self.levels_polling = true;
            engine.eval(View::Player, scripts::LEVELS, Purpose::Levels);
        } else if self.levels_polling && self.last_levels_poll.elapsed() >= Duration::from_secs(2) {
            // A lost reply (page reloaded) must not stop the meter for good.
            self.levels_polling = false;
        }
    }

    // ----- account page and API transport -----------------------------------

    fn refresh_auth(&mut self, engine: &mut dyn Engine) {
        if !self.account_ready {
            return;
        }
        if host_of(&self.account_url) != "music.youtube.com" {
            self.set_auth_state("checking", engine);
            return;
        }
        if let Some(started) = self.auth_polling {
            if started.elapsed() < Duration::from_secs(5) {
                return;
            }
            self.set_auth_state("unknown", engine);
        }
        self.auth_polling = Some(Instant::now());
        self.last_auth_poll = Instant::now();
        engine.eval(View::Account, scripts::AUTH, Purpose::Auth { generation: self.auth_generation });
    }

    fn request(&mut self, id: String, endpoint: String, body: Value, engine: &mut dyn Engine) {
        if host_of(&self.account_url) != "music.youtube.com" || !self.account_ready {
            self.emit(Event::Response { id, data: None, error: Some("yt_open_account".into()) });
            return;
        }
        if !matches!(endpoint.as_str(), "search" | "browse") || self.pending.len() >= 4 {
            self.emit(Event::Response { id, data: None, error: Some("yt_busy".into()) });
            return;
        }
        self.pending.insert(id.clone(), Pending { deadline: Instant::now() + Duration::from_secs(20) });
        // Read only public client configuration, never account tokens or cookies.
        engine.eval(View::Account, scripts::CONFIG, Purpose::RequestConfig { id, endpoint, body });
    }

    fn configured(&mut self, id: String, endpoint: String, body: Value, raw: &str, engine: &mut dyn Engine) {
        if !self.pending.contains_key(&id) {
            return;
        }
        let config: Value = unwrap_json_string(raw).unwrap_or(Value::Null);
        let logged_in = config.get("loggedIn").and_then(Value::as_bool).unwrap_or(false);
        if body.get("browseId").and_then(Value::as_str) == Some("FEmusic_liked_playlists") && !logged_in {
            self.finish(&id, Err("yt_auth_error"));
            return;
        }
        let version = config
            .get("context")
            .and_then(|c| c.get("client"))
            .and_then(|c| c.get("clientVersion"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && v.len() <= 100)
            .map(str::to_string);
        let Some(version) = version else {
            self.finish(&id, Err("yt_open_account"));
            return;
        };
        let account: String = config.get("account").and_then(Value::as_str).unwrap_or("0").chars().take(10).collect();
        let mut body = body;
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "context".into(),
                json!({"client": {"clientName": "WEB_REMIX", "clientVersion": version, "hl": self.language}}),
            );
        }
        let payload = json!({"id": id, "endpoint": endpoint, "account": account, "body": body});
        let script = scripts::REQUEST.replace("REQUEST", &payload.to_string());
        engine.run(View::Account, &script);
    }

    fn finish(&mut self, id: &str, result: Result<Value, &str>) {
        if self.pending.remove(id).is_none() {
            return;
        }
        match result {
            Ok(data) => self.emit(Event::Response { id: id.to_string(), data: Some(data), error: None }),
            Err(key) => {
                let key = if matches!(key, "yt_auth_error" | "yt_network_error" | "yt_open_account") {
                    key
                } else {
                    "yt_network_error"
                };
                self.emit(Event::Response { id: id.to_string(), data: None, error: Some(key.to_string()) });
            }
        }
    }

    fn cancel_requests(&mut self, engine: &mut dyn Engine) {
        let ids: Vec<String> = self.pending.keys().cloned().collect();
        self.pending.clear();
        self.request_polling = false;
        engine.run(View::Account, scripts::ABORT_ALL);
        for id in ids {
            self.emit(Event::Response { id, data: None, error: Some("yt_cancelled".into()) });
        }
    }

    fn poll_requests(&mut self, engine: &mut dyn Engine) {
        let now = Instant::now();
        let expired: Vec<String> = self.pending.iter().filter(|(_, p)| now > p.deadline).map(|(id, _)| id.clone()).collect();
        for id in expired {
            self.finish(&id, Err("yt_network_error"));
            engine.run(View::Account, &format!("globalThis.__fono8Requests?.[{}]?.controller?.abort();", json!(id)));
        }
        if self.request_polling || self.pending.is_empty() {
            return;
        }
        self.request_polling = true;
        engine.eval(View::Account, scripts::POLL, Purpose::RequestPoll);
    }

    // ----- player -----------------------------------------------------------

    fn player_command(&mut self, command: &str, engine: &mut dyn Engine) {
        engine.run(
            View::Player,
            &format!("(() => {{if(location.origin !== 'https://music.youtube.com') return;const p=document.getElementById('movie_player'); if(p) {{{command}}}}})()"),
        );
    }

    fn apply_mute(&mut self, engine: &mut dyn Engine) {
        let muted = self.muted || self.target.is_none() || !self.start_requested;
        let command =
            format!("const v=document.querySelector('video'); if(v) v.muted={muted}; if({muted}) p.mute?.(); else p.unMute?.();");
        self.player_command(&command, engine);
    }

    fn set_player_state(&mut self, state: &'static str) {
        if self.player_state != state {
            self.player_state = state;
            self.emit_timeline();
        }
    }

    fn emit_timeline(&mut self) {
        let event = Event::Player { state: self.player_state.into(), position: self.position, duration: self.duration };
        self.emit(event);
    }

    fn load(&mut self, video: String, engine: &mut dyn Engine) {
        self.stop_player(engine);
        if self.auth_state == "signed_out" {
            self.emit(Event::Error { key: "yt_login_required".into() });
            return;
        }
        if !is_video_id(&video) {
            self.emit(Event::Error { key: "yt_playback_error".into() });
            return;
        }
        self.target = Some(video.clone());
        self.started = false;
        self.start_requested = false;
        self.waiting_for_load = true;
        self.want_play = true;
        self.deadline = Instant::now() + Duration::from_secs(45);
        self.position = 0;
        self.duration = 0;
        self.emit_timeline();
        self.apply_mute(engine);
        engine.load_url(View::Player, &format!("{ORIGIN}/watch?v={video}"));
    }

    fn play(&mut self, engine: &mut dyn Engine) {
        self.want_play = true;
        if !self.waiting_for_load {
            self.player_command("p.playVideo();", engine);
        }
    }

    fn pause(&mut self, engine: &mut dyn Engine) {
        self.want_play = false;
        self.player_command("p.pauseVideo();", engine);
        self.set_player_state("paused");
    }

    pub fn stop_player(&mut self, engine: &mut dyn Engine) {
        self.player_generation += 1;
        self.target = None;
        self.waiting_for_load = false;
        self.player_polling = false;
        self.pending_seek = None;
        self.player_command("window.clearInterval?.(window.__fono8Playback?.timer); p.pauseVideo();", engine);
        self.apply_mute(engine);
        self.set_player_state("stopped");
    }

    fn seek(&mut self, ms: u64, engine: &mut dyn Engine) {
        let Some(target) = self.target.clone() else { return };
        if self.waiting_for_load {
            return;
        }
        // Old JS replies and buffering positions must not undo a user's seek.
        self.player_generation += 1;
        self.player_polling = false;
        self.pending_seek = Some(ms);
        self.seek_started = Instant::now();
        self.seek_deadline = self.seek_started + Duration::from_secs(5);
        self.deadline = self.seek_started + Duration::from_secs(45);
        self.position = ms;
        self.emit_timeline();
        let params = json!({"target": target, "position": ms, "epoch": self.player_generation});
        engine.run(View::Player, &scripts::SEEK.replace("PARAMETERS", &params.to_string()));
    }

    fn player_url_changed(&mut self, url: &str, engine: &mut dyn Engine) {
        let Some(target) = self.target.clone() else { return };
        if host_of(url) != "music.youtube.com" || path_of(url) != "/watch" {
            // Login redirects or manual browsing are not a failed song.
            self.stop_player(engine);
            self.emit(Event::Error { key: "yt_playback_interrupted".into() });
        } else if self.started {
            if let Some(id) = query_param(url, "v") {
                if id != target {
                    // Website autoplay navigated to another song before a poll noticed.
                    self.stop_player(engine);
                    self.emit(Event::Ended);
                }
            }
        }
    }

    fn poll_player(&mut self, engine: &mut dyn Engine) {
        if self.target.is_none() {
            return;
        }
        if self.auth_state == "signed_out" {
            self.stop_player(engine);
            self.emit(Event::Error { key: "yt_login_required".into() });
            return;
        }
        if Instant::now() > self.deadline {
            let key = if self.auth_state == "signed_in" { "yt_playback_error" } else { "yt_auth_check_failed" };
            self.stop_player(engine);
            self.emit(Event::Error { key: key.into() });
            return;
        }
        if self.waiting_for_load || self.player_polling || (!self.started && self.auth_state != "signed_in") {
            return;
        }
        self.player_polling = true;
        let params = json!({"target": self.target, "started": self.started, "epoch": self.player_generation});
        engine.eval(
            View::Player,
            &scripts::STATE.replace("PARAMETERS", &params.to_string()),
            Purpose::PlayerState { generation: self.player_generation },
        );
    }

    fn player_state_reply(&mut self, generation: u64, raw: &str, engine: &mut dyn Engine) {
        if generation != self.player_generation || self.target.is_none() {
            return;
        }
        self.player_polling = false;
        let Some(data) = unwrap_json_string(raw) else { return };
        let target = self.target.clone().unwrap_or_default();
        let id = data.get("id").and_then(Value::as_str).unwrap_or("");
        if id != target {
            if self.started && !id.is_empty() {
                // YTM advanced its queue. Fono8 owns the mixed queue.
                self.stop_player(engine);
                self.emit(Event::Ended);
            }
            return;
        }
        let state = data.get("state").and_then(Value::as_i64);
        let ended = data.get("ended").and_then(Value::as_bool).unwrap_or(false);
        if self.started && (ended || state == Some(0)) {
            self.stop_player(engine);
            self.emit(Event::Ended);
            return;
        }
        let position = data.get("position").and_then(Value::as_f64).unwrap_or(0.0) * 1000.0;
        if let Some(requested) = self.pending_seek {
            let elapsed = if self.player_state == "playing" { self.seek_started.elapsed().as_millis() as f64 } else { 0.0 };
            let seeking = data.get("seeking").and_then(Value::as_bool).unwrap_or(false);
            let settled = position.is_finite()
                && position >= requested as f64 - 750.0
                && position <= requested as f64 + elapsed + 750.0
                && !seeking
                && matches!(state, Some(0) | Some(1) | Some(2));
            if !settled && Instant::now() < self.seek_deadline {
                return;
            }
            self.pending_seek = None;
        }
        if !self.start_requested {
            self.start_requested = true;
            self.apply_mute(engine);
            let command = format!("p.setVolume({});", self.volume * 100.0);
            self.player_command(&command, engine);
            if self.want_play {
                self.play(engine);
            } else {
                self.pause(engine);
            }
        }
        match state {
            Some(1) => {
                self.started = true;
                self.deadline = Instant::now() + Duration::from_secs(45);
                self.set_player_state("playing");
            }
            Some(2) => {
                if self.started || !self.want_play {
                    self.deadline = Instant::now() + Duration::from_secs(45);
                }
                self.set_player_state("paused");
            }
            _ => {}
        }
        let mut changed = false;
        if position.is_finite() && (0.0..=2_000_000_000.0).contains(&position) {
            let value = position as u64;
            if value != self.position {
                self.position = value;
                changed = true;
            }
        }
        let duration = data.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        // YTM can extend the media timeline when preparing its next song; keep the
        // duration stable through seeks and reset it only for another queue entry.
        if self.duration == 0
            && duration > 0.0
            && duration.is_finite()
            && duration <= 2_000_000.0
            && matches!(state, Some(1) | Some(2))
        {
            self.duration = (duration * 1000.0) as u64;
            changed = true;
        }
        if changed {
            self.emit_timeline();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeEngine {
        evals: Vec<(View, Purpose)>,
        runs: Vec<(View, String)>,
        loads: Vec<(View, String)>,
    }

    impl Engine for FakeEngine {
        fn eval(&mut self, view: View, _script: &str, purpose: Purpose) {
            self.evals.push((view, purpose));
        }
        fn run(&mut self, view: View, script: &str) {
            self.runs.push((view, script.to_string()));
        }
        fn load_url(&mut self, view: View, url: &str) {
            self.loads.push((view, url.to_string()));
        }
        fn show(&mut self, _view: View) {}
        fn hide(&mut self) {}
        fn toolbar(&mut self, _state: &ToolbarState) {}
    }

    #[test]
    fn navigation_policy_matches_the_qt_build() {
        assert_eq!(navigation_error("https://music.youtube.com/watch?v=abc"), None);
        assert_eq!(navigation_error("https://accounts.google.com/signin"), None);
        assert_eq!(navigation_error("about:blank"), None);
        assert_eq!(navigation_error("http://music.youtube.com/"), Some("yt_nav_protocol"));
        assert_eq!(navigation_error("file:///etc/passwd"), Some("yt_nav_protocol"));
        assert_eq!(navigation_error("https://evil.example/"), Some("yt_nav_host"));
        assert_eq!(navigation_error("https://music.youtube.com.evil.example/"), Some("yt_nav_host"));
        assert_eq!(navigation_error("https://music.youtube.com:8443/"), Some("yt_nav_port"));
        assert_eq!(navigation_error("https://user@music.youtube.com/"), Some("yt_nav_credentials"));
        assert_eq!(navigation_error("garbage"), Some("yt_nav_invalid"));
    }

    #[test]
    fn url_helpers() {
        let url = "https://music.youtube.com/watch?v=abcdefghijk&list=RD";
        assert_eq!(host_of(url), "music.youtube.com");
        assert_eq!(path_of(url), "/watch");
        assert_eq!(query_param(url, "v").as_deref(), Some("abcdefghijk"));
        assert_eq!(query_param(url, "x"), None);
        assert_eq!(unwrap_json_string("\"{\\\"a\\\":1}\""), Some(json!({"a": 1})));
        assert_eq!(unwrap_json_string("\"signed_in\""), Some(json!("signed_in")));
    }

    #[test]
    fn request_needs_a_loaded_music_page_and_runs_in_the_page() {
        let mut engine = FakeEngine::default();
        let mut core = Core::new(true, "en");
        core.start(&mut engine);
        assert_eq!(core.take_events(), vec![Event::Ready { persistent: true }]);
        core.command(Command::Request { id: "r1".into(), endpoint: "search".into(), body: json!({"query": "x"}) }, &mut engine);
        assert_eq!(
            core.take_events(),
            vec![Event::Response { id: "r1".into(), data: None, error: Some("yt_open_account".into()) }]
        );
        core.page_load(View::Account, PageLoad::Started, ORIGIN, &mut engine);
        core.page_load(View::Account, PageLoad::Finished, ORIGIN, &mut engine);
        assert!(matches!(engine.evals.last(), Some((View::Account, Purpose::Auth { .. }))));
        core.command(Command::Request { id: "r2".into(), endpoint: "search".into(), body: json!({"query": "x"}) }, &mut engine);
        let (_, purpose) = engine.evals.last().cloned().unwrap();
        assert!(matches!(purpose, Purpose::RequestConfig { .. }));
        let config = json!({"context": {"client": {"clientVersion": "1.2"}}, "account": "0", "loggedIn": false}).to_string();
        core.script_reply(purpose, &serde_json::to_string(&config).unwrap(), &mut engine);
        let script = &engine.runs.last().unwrap().1;
        assert!(script.contains("WEB_REMIX") && script.contains("\"id\":\"r2\""));
        core.last_request_poll = Instant::now() - Duration::from_secs(1);
        core.tick(&mut engine);
        let (_, poll) = engine.evals.last().cloned().unwrap();
        assert!(matches!(poll, Purpose::RequestPoll));
        let completed = json!({"r2": {"data": {"contents": {}}}}).to_string();
        core.script_reply(poll, &serde_json::to_string(&completed).unwrap(), &mut engine);
        let events = core.take_events();
        assert!(events.iter().any(|e| matches!(e, Event::Response { id, data: Some(_), .. } if id == "r2")), "{events:?}");
    }

    #[test]
    fn player_load_waits_for_the_watch_page_and_reports_end_once() {
        let mut engine = FakeEngine::default();
        let mut core = Core::new(true, "en");
        core.start(&mut engine);
        core.page_load(View::Account, PageLoad::Started, ORIGIN, &mut engine);
        core.page_load(View::Account, PageLoad::Finished, ORIGIN, &mut engine);
        let (_, auth) = engine.evals.last().cloned().unwrap();
        core.script_reply(auth, "\"signed_in\"", &mut engine);
        core.command(Command::Load { video: "abcdefghijk".into() }, &mut engine);
        assert_eq!(engine.loads.last().unwrap().1, "https://music.youtube.com/watch?v=abcdefghijk");
        let watch = "https://music.youtube.com/watch?v=abcdefghijk";
        core.page_load(View::Player, PageLoad::Started, watch, &mut engine);
        core.page_load(View::Player, PageLoad::Finished, watch, &mut engine);
        core.last_player_poll = Instant::now() - Duration::from_secs(1);
        core.tick(&mut engine);
        let (_, state) = engine.evals.last().cloned().unwrap();
        assert!(matches!(state, Purpose::PlayerState { .. }));
        let playing = json!({"id": "abcdefghijk", "state": 1, "position": 3.5, "duration": 200.0}).to_string();
        core.script_reply(state.clone(), &serde_json::to_string(&playing).unwrap(), &mut engine);
        let events = core.take_events();
        assert!(
            events.iter().any(|e| matches!(e, Event::Player { state, position: 3500, duration: 200000 } if state == "playing")),
            "{events:?}"
        );
        // While playing, every tick also measures the band levels.
        core.last_player_poll = Instant::now() - Duration::from_secs(1);
        core.tick(&mut engine);
        let (_, levels) = engine.evals.iter().rev().find(|(_, p)| matches!(p, Purpose::Levels)).cloned().unwrap();
        core.script_reply(levels, "[-20.5, -30, -35, -40, -50]", &mut engine);
        let events = core.take_events();
        assert!(events.iter().any(|e| matches!(e, Event::Levels { bands } if bands.len() == 5)), "{events:?}");
        let ended = json!({"id": "abcdefghijk", "state": 0, "position": 200.0, "duration": 200.0, "ended": true}).to_string();
        let (_, state) = engine.evals.iter().rev().find(|(_, p)| matches!(p, Purpose::PlayerState { .. })).cloned().unwrap();
        core.script_reply(state, &serde_json::to_string(&ended).unwrap(), &mut engine);
        let events = core.take_events();
        assert_eq!(events.iter().filter(|e| matches!(e, Event::Ended)).count(), 1, "{events:?}");
        let shared = Rc::new(RefCell::new(0));
        let _ = shared;
    }
}
