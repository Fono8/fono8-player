//! The Spotify worker: one background thread owns the credentials, the loopback
//! server and all Web API traffic. The UI sends [`Request`]s and drains
//! [`Update`]s on its tick, so no call ever blocks rendering. Tokens never
//! leave this thread except as [`Update::Token`] for the Web Playback SDK.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::i18n::Message;
use crate::library::Track;

use crate::keystore::TokenStore;
use crate::net::{Body, Method, Response, Transport};
use crate::oauth::{CallError, Notice, Session};

use super::api::{self, Page, Playlist, Profile};
use super::auth::PROVIDER;

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Start the browser sign-in with this Client ID.
    SignIn {
        client_id: String,
    },
    /// Resume the session saved in the keyring for this Client ID, if any.
    Restore {
        client_id: String,
    },
    Search {
        query: String,
        offset: u32,
    },
    Playlists {
        offset: u32,
    },
    /// All tracks of a playlist the user owns or collaborates on (bounded).
    Import {
        playlist: String,
        enqueue: bool,
    },
    /// All liked songs (bounded).
    Liked {
        enqueue: bool,
    },
    /// A fresh access token for the Web Playback SDK.
    Token,
    /// The loopback player page URL (starts the server if needed).
    PlayerUrl,
    Play {
        device: String,
        uri: String,
        position_ms: u64,
    },
    SignOut,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    OpenUrl(String),
    Status(Message),
    Failed(Message),
    SignedIn(Profile),
    SignedOut,
    Results { query: String, offset: u32, page: Page<Track> },
    Playlists(Page<Playlist>),
    Imported { name: String, tracks: Vec<Track>, enqueue: bool },
    Token(Option<String>),
    PlayerUrl(String),
    Playing,
}

enum Input {
    Request(Request),
    /// A request whose updates go back to the caller (the player engine).
    Direct(Request, Sender<Update>),
    Callback(String),
    Quit,
}

pub struct Worker {
    session: Session,
    /// The signed-in user's id, for telling owned playlists apart.
    user: String,
    updates: Vec<Update>,
}

impl Worker {
    fn new(
        transport: Box<dyn Transport>,
        store: Box<dyn TokenStore>,
        port: u16,
        done_text: String,
        inbox: Sender<Input>,
    ) -> Worker {
        let inbox = Mutex::new(inbox);
        let session = Session::new(&PROVIDER, transport, store, port, done_text, move |target| {
            if let Ok(inbox) = inbox.lock() {
                let _ = inbox.send(Input::Callback(target));
            }
        });
        Worker { session, user: String::new(), updates: Vec::new() }
    }

    fn emit(&mut self, update: Update) {
        self.updates.push(update);
    }

    /// Report what the session noticed, in order.
    fn drain(&mut self) {
        for notice in self.session.take_notices() {
            self.updates.push(match notice {
                Notice::Status(message) => Update::Status(message),
                Notice::Failed(message) => Update::Failed(message),
                Notice::SignedOut => Update::SignedOut,
            });
        }
    }

    fn take_updates(&mut self) -> Vec<Update> {
        self.drain();
        std::mem::take(&mut self.updates)
    }

    fn handle(&mut self, input: Input, now: Instant) {
        match input {
            Input::Callback(target) => {
                if self.session.callback(&target, now) {
                    self.drain();
                    self.signed_in(now);
                }
            }
            Input::Request(request) | Input::Direct(request, _) => self.request(request, now),
            Input::Quit => {}
        }
        self.drain();
    }

    fn tick(&mut self, now: Instant) {
        self.session.tick(now);
        self.drain();
    }

    fn request(&mut self, request: Request, now: Instant) {
        match request {
            Request::SignIn { client_id } => {
                if let Some(url) = self.session.begin(&client_id, now) {
                    self.emit(Update::OpenUrl(url));
                    self.emit(Update::Status(Message::new("spotify_login_waiting")));
                }
            }
            Request::Restore { client_id } => {
                if self.session.restore(&client_id, now) {
                    self.drain();
                    self.signed_in(now);
                }
            }
            Request::Search { query, offset } => {
                let query = query.trim().to_string();
                if query.is_empty() || query.chars().count() > 500 {
                    return self.emit(Update::Failed(Message::new("spotify_invalid_query")));
                }
                if let Some(response) = self.call(Method::Get, &api::search_url(&query, offset), None, now) {
                    self.emit(Update::Results { query, offset, page: api::parse_search(&response.body) });
                }
            }
            Request::Playlists { offset } => {
                if let Some(response) = self.call(Method::Get, &api::playlists_url(offset), None, now) {
                    let page = api::parse_playlists(&response.body, &self.user);
                    self.emit(Update::Playlists(page));
                }
            }
            Request::Import { playlist, enqueue } => {
                let Ok(id) = super::parse_playlist(&playlist).or_else(|_| {
                    if super::is_id(&playlist) {
                        Ok(playlist.clone())
                    } else {
                        Err(())
                    }
                }) else {
                    return self.emit(Update::Failed(Message::new("spotify_playlist_invalid")));
                };
                let Some(response) = self.call(Method::Get, &api::playlist_url(&id), None, now) else { return };
                let name = api::parse_playlist_name(&response.body);
                if let Some(tracks) =
                    self.collect(|offset| api::playlist_items_url(&id, offset), "spotify_playlist_not_owned", now)
                {
                    let name = if name.is_empty() { id } else { name };
                    self.emit(Update::Imported { name, tracks, enqueue });
                }
            }
            Request::Liked { enqueue } => {
                if let Some(tracks) = self.collect(api::saved_tracks_url, "spotify_access_denied", now) {
                    self.emit(Update::Imported { name: String::new(), tracks, enqueue });
                }
            }
            Request::Token => {
                let token = self.session.token(now);
                self.drain();
                self.emit(Update::Token(token));
            }
            Request::PlayerUrl => {
                if let Some(server) = self.session.server() {
                    self.emit(Update::PlayerUrl(server.player_url()));
                }
            }
            Request::Play { device, uri, position_ms } => {
                let body = json!({"uris": [uri], "position_ms": position_ms});
                if self.call(Method::Put, &api::play_url(&device), Some(Body::Json(body)), now).is_some() {
                    // Fono8 owns the queue: one track at a time, Spotify must not repeat or continue.
                    let _ = self.call(Method::Put, &api::repeat_off_url(&device), None, now);
                    let _ = self.call(Method::Put, &api::shuffle_off_url(&device), None, now);
                    self.emit(Update::Playing);
                }
            }
            Request::SignOut => {
                self.session.sign_out();
                self.user.clear();
                self.emit(Update::SignedOut);
                self.emit(Update::Status(Message::new("spotify_disconnected")));
            }
        }
    }

    fn signed_in(&mut self, now: Instant) {
        // In development mode Spotify answers 403 for accounts that are not on the app's
        // user list (Developer Dashboard > User Management).
        if let Some(response) = self.call_or(Method::Get, &api::me_url(), None, "spotify_user_not_allowed", now) {
            let profile = api::parse_profile(&response.body);
            self.user = profile.id.clone();
            if profile.premium == Some(false) {
                self.emit(Update::Status(Message::new("spotify_premium_required")));
            }
            self.emit(Update::SignedIn(profile));
        }
    }

    /// An authorized API call; refreshes once on 401. Failures are reported and give `None`.
    fn call(&mut self, method: Method, url: &str, body: Option<Body>, now: Instant) -> Option<Response> {
        self.call_or(method, url, body, "spotify_access_denied", now)
    }

    /// Like [`Self::call`], reporting a 403 reply as `forbidden`.
    fn call_or(
        &mut self,
        method: Method,
        url: &str,
        body: Option<Body>,
        forbidden: &'static str,
        now: Instant,
    ) -> Option<Response> {
        let result = self.session.call(method, url, body, now);
        self.drain();
        match result {
            Ok(response) => Some(response),
            Err(CallError::Http(response)) => {
                // Only the path: queries can hold search terms or device ids.
                let path = url.split('?').next().unwrap_or(url);
                crate::app::debug(|| format!("spotify: {method:?} {path}: HTTP {}", response.status));
                let message = if response.status == 403 { Message::new(forbidden) } else { api::error(&response) };
                self.emit(Update::Failed(message));
                None
            }
            Err(CallError::Auth) => None,
        }
    }

    /// Follow `next` offsets of a paged track listing up to [`api::MAX_IMPORT`] tracks.
    fn collect(&mut self, url: impl Fn(u32) -> String, forbidden: &'static str, now: Instant) -> Option<Vec<Track>> {
        let mut tracks: Vec<Track> = Vec::new();
        let mut offset = 0;
        loop {
            let response = self.call_or(Method::Get, &url(offset), None, forbidden, now)?;
            let page = api::parse_wrapped_tracks(&response.body);
            for track in page.items {
                if !tracks.iter().any(|t| t.path == track.path) {
                    tracks.push(track);
                }
            }
            match page.next {
                Some(next) if next > offset && tracks.len() < api::MAX_IMPORT => offset = next,
                _ => break,
            }
        }
        tracks.truncate(api::MAX_IMPORT);
        Some(tracks)
    }
}

/// Handle owned by the UI model.
pub struct Spotify {
    inbox: Sender<Input>,
    updates: Receiver<Update>,
    thread: Option<JoinHandle<()>>,
}

impl Spotify {
    pub fn start(transport: Box<dyn Transport>, store: Box<dyn TokenStore>, port: u16, done_text: String) -> Spotify {
        let (inbox, requests) = mpsc::channel::<Input>();
        let (updates_sender, updates) = mpsc::channel();
        let mut worker = Worker::new(transport, store, port, done_text, inbox.clone());
        let thread = std::thread::Builder::new()
            .name("fono8-spotify".into())
            .spawn(move || loop {
                let input = requests.recv_timeout(Duration::from_millis(500));
                let quit = matches!(input, Ok(Input::Quit) | Err(RecvTimeoutError::Disconnected));
                match input {
                    Ok(Input::Direct(request, reply)) => {
                        worker.request(request, Instant::now());
                        for update in worker.take_updates() {
                            // The UI must still learn that the session ended.
                            if update == Update::SignedOut {
                                let _ = updates_sender.send(update.clone());
                            }
                            let _ = reply.send(update);
                        }
                    }
                    Ok(input) => worker.handle(input, Instant::now()),
                    Err(_) => {}
                }
                worker.tick(Instant::now());
                for update in worker.take_updates() {
                    if updates_sender.send(update).is_err() {
                        return;
                    }
                }
                if quit {
                    worker.session.sign_out_pending();
                    return;
                }
            })
            .ok();
        Spotify { inbox, updates, thread }
    }

    pub fn send(&self, request: Request) {
        let _ = self.inbox.send(Input::Request(request));
    }

    /// A handle for other threads that need replies of their own.
    pub fn link(&self) -> ClientLink {
        ClientLink { inbox: self.inbox.clone() }
    }

    pub fn poll(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }

    /// Wait for the next update (tests).
    #[cfg(test)]
    pub fn wait(&self, timeout: Duration) -> Option<Update> {
        self.updates.recv_timeout(timeout).ok()
    }
}

/// Sends requests to the worker and waits for their updates (used off the UI thread).
#[derive(Clone)]
pub struct ClientLink {
    inbox: Sender<Input>,
}

impl ClientLink {
    /// All updates produced by `request`, or nothing if the worker is gone or too slow.
    pub fn call(&self, request: Request, timeout: Duration) -> Vec<Update> {
        let (reply, updates) = mpsc::channel();
        if self.inbox.send(Input::Direct(request, reply)).is_err() {
            return Vec::new();
        }
        let deadline = Instant::now() + timeout;
        let mut received = Vec::new();
        loop {
            match updates.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(update) => received.push(update),
                Err(_) => return received, // the worker dropped the sender: done
            }
        }
    }
}

impl Drop for Spotify {
    fn drop(&mut self) {
        let _ = self.inbox.send(Input::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::Memory;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Default)]
    struct Fake {
        replies: Mutex<VecDeque<(&'static str, Response)>>,
        log: Mutex<Vec<(Method, String, Option<String>, Option<Body>)>>,
    }

    struct FakeTransport(Arc<Fake>);

    impl Transport for FakeTransport {
        fn request(&self, method: Method, url: &str, bearer: Option<&str>, body: Option<Body>) -> Response {
            self.0.log.lock().unwrap().push((method, url.to_string(), bearer.map(str::to_string), body));
            let mut replies = self.0.replies.lock().unwrap();
            match replies.front() {
                Some((prefix, _)) if url.starts_with(prefix) => replies.pop_front().unwrap().1,
                other => panic!("unexpected request {url}, next reply is for {:?}", other.map(|r| r.0)),
            }
        }
    }

    struct SharedStore(Arc<Memory>);

    impl TokenStore for SharedStore {
        fn load(&self, client_id: &str) -> Option<String> {
            self.0.load(client_id)
        }
        fn save(&self, client_id: &str, token: &str) -> bool {
            self.0.save(client_id, token)
        }
        fn delete(&self, client_id: &str) {
            self.0.delete(client_id)
        }
    }

    const CLIENT: &str = "0123456789abcdef0123456789abcdef";

    fn reply(status: u16, body: Value) -> Response {
        Response { status, body, retry_after: None }
    }

    fn worker(store: Memory) -> (Worker, Arc<Fake>, Arc<Memory>) {
        let fake = Arc::new(Fake::default());
        let store = Arc::new(store);
        let (inbox, _requests) = mpsc::channel();
        let worker =
            Worker::new(Box::new(FakeTransport(fake.clone())), Box::new(SharedStore(store.clone())), 0, "ok".into(), inbox);
        (worker, fake, store)
    }

    fn expect(fake: &Fake, prefix: &'static str, response: Response) {
        fake.replies.lock().unwrap().push_back((prefix, response));
    }

    fn token_reply(access: &str, refresh: Option<&str>) -> Response {
        let mut body = json!({"access_token": access, "expires_in": 3600});
        if let Some(refresh) = refresh {
            body["refresh_token"] = json!(refresh);
        }
        reply(200, body)
    }

    #[test]
    fn sign_in_exchanges_code_saves_refresh_token_and_loads_profile() {
        let now = Instant::now();
        let (mut w, fake, store) = worker(Memory::default());
        w.handle(Input::Request(Request::SignIn { client_id: CLIENT.into() }), now);
        let updates = w.take_updates();
        let Some(Update::OpenUrl(url)) = updates.first() else { panic!("{updates:?}") };
        assert!(url.starts_with("https://accounts.spotify.com/authorize?"));
        let state = w.session.auth.expected_state().unwrap();
        assert!(w.session.server().is_some_and(|s| s.port() != 0));

        expect(&fake, PROVIDER.token_url, token_reply("access", Some("refresh")));
        expect(&fake, "https://api.spotify.com/v1/me", reply(200, json!({"display_name": "Michał", "product": "premium"})));
        w.handle(Input::Callback(format!("/callback?code=abc&state={state}")), now);
        assert_eq!(
            w.take_updates(),
            [Update::SignedIn(Profile { id: String::new(), name: "Michał".into(), premium: Some(true) })]
        );
        assert_eq!(store.load(CLIENT).as_deref(), Some("refresh"));
        let log = fake.log.lock().unwrap();
        assert_eq!(log[0].2, None, "token endpoint gets no bearer");
        assert_eq!(log[1].2.as_deref(), Some("access"));
    }

    #[test]
    fn an_account_outside_the_app_user_list_gets_a_clear_message() {
        let now = Instant::now();
        let (mut w, fake, _store) = worker(Memory::default());
        w.handle(Input::Request(Request::SignIn { client_id: CLIENT.into() }), now);
        w.take_updates();
        let state = w.session.auth.expected_state().unwrap();
        expect(&fake, PROVIDER.token_url, token_reply("access", Some("refresh")));
        expect(&fake, "https://api.spotify.com/v1/me", reply(403, json!({"error": {"status": 403}})));
        w.handle(Input::Callback(format!("/callback?code=abc&state={state}")), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_user_not_allowed"))]);
    }

    #[test]
    fn restore_refreshes_and_expired_session_is_forgotten() {
        let now = Instant::now();
        let memory = Memory::default();
        memory.save(CLIENT, "saved");
        let (mut w, fake, store) = worker(memory);
        expect(&fake, PROVIDER.token_url, token_reply("access", Some("rotated")));
        expect(&fake, "https://api.spotify.com/v1/me", reply(200, json!({"display_name": "M"})));
        w.handle(Input::Request(Request::Restore { client_id: CLIENT.into() }), now);
        assert_eq!(w.take_updates(), [Update::SignedIn(Profile { id: String::new(), name: "M".into(), premium: None })]);
        assert_eq!(store.load(CLIENT).as_deref(), Some("rotated"), "rotated refresh token is saved");

        let memory = Memory::default();
        memory.save(CLIENT, "too-old");
        let (mut w, fake, store) = worker(memory);
        expect(&fake, PROVIDER.token_url, reply(400, json!({"error": "invalid_grant"})));
        w.handle(Input::Request(Request::Restore { client_id: CLIENT.into() }), now);
        assert_eq!(w.take_updates(), [Update::SignedOut, Update::Failed(Message::new("spotify_session_expired"))]);
        assert_eq!(store.load(CLIENT), None);

        let (mut w, fake, _) = worker(Memory::default());
        w.handle(Input::Request(Request::Restore { client_id: CLIENT.into() }), now);
        assert!(w.take_updates().is_empty() && fake.log.lock().unwrap().is_empty(), "nothing saved, nothing sent");
    }

    #[test]
    fn api_call_refreshes_once_on_401_and_reports_other_errors() {
        let now = Instant::now();
        let memory = Memory::default();
        memory.save(CLIENT, "saved");
        let (mut w, fake, _) = worker(memory);
        w.session.auth.restore(CLIENT, "saved".into());
        expect(&fake, PROVIDER.token_url, token_reply("first", None));
        expect(&fake, "https://api.spotify.com/v1/search", reply(401, Value::Null));
        expect(&fake, PROVIDER.token_url, token_reply("second", None));
        let track = json!({"type": "track", "id": "0DiWol3AO6WpXZgp0goxAV", "name": "One More Time", "artists": [], "album": {}});
        expect(
            &fake,
            "https://api.spotify.com/v1/search",
            reply(200, json!({"tracks": {"items": [track], "total": 1, "offset": 0, "limit": 10, "next": null}})),
        );
        w.handle(Input::Request(Request::Search { query: " daft punk ".into(), offset: 0 }), now);
        let updates = w.take_updates();
        let [Update::Results { query, offset: 0, page }] = updates.as_slice() else { panic!("{updates:?}") };
        assert_eq!((query.as_str(), page.items.len()), ("daft punk", 1));
        let bearers: Vec<_> = fake.log.lock().unwrap().iter().map(|r| r.2.clone()).collect();
        assert_eq!(bearers, [None, Some("first".into()), None, Some("second".into())]);

        expect(&fake, "https://api.spotify.com/v1/me/playlists", reply(429, Value::Null));
        w.handle(Input::Request(Request::Playlists { offset: 0 }), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_rate_limit"))]);
        w.handle(Input::Request(Request::Search { query: "  ".into(), offset: 0 }), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_invalid_query"))]);
    }

    #[test]
    fn import_follows_pages_dedupes_and_play_pins_repeat_and_shuffle_off() {
        let now = Instant::now();
        let (mut w, fake, _) = worker(Memory::default());
        w.session.auth.restore(CLIENT, "saved".into());
        let track = |id: &str| json!({"item": {"type": "track", "id": id, "name": id, "artists": [], "album": {}}});
        expect(&fake, PROVIDER.token_url, token_reply("access", None));
        expect(&fake, "https://api.spotify.com/v1/playlists/37i9dQZF1DXcBWIGoYBM5M?", reply(200, json!({"name": "Mix"})));
        expect(
            &fake,
            "https://api.spotify.com/v1/playlists/37i9dQZF1DXcBWIGoYBM5M/items?limit=50&offset=0",
            reply(
                200,
                json!({"items": [track("0DiWol3AO6WpXZgp0goxAV"), track("0DiWol3AO6WpXZgp0goxAW")],
                    "total": 3, "offset": 0, "limit": 2, "next": "more"}),
            ),
        );
        expect(
            &fake,
            "https://api.spotify.com/v1/playlists/37i9dQZF1DXcBWIGoYBM5M/items?limit=50&offset=2",
            reply(200, json!({"items": [track("0DiWol3AO6WpXZgp0goxAV")], "total": 3, "offset": 2, "limit": 2, "next": null})),
        );
        w.handle(
            Input::Request(Request::Import {
                playlist: "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M".into(),
                enqueue: true,
            }),
            now,
        );
        let updates = w.take_updates();
        let [Update::Imported { name, tracks, enqueue: true }] = updates.as_slice() else { panic!("{updates:?}") };
        assert_eq!((name.as_str(), tracks.len()), ("Mix", 2));

        w.handle(
            Input::Request(Request::Import { playlist: "spotify:album:37i9dQZF1DXcBWIGoYBM5M".into(), enqueue: false }),
            now,
        );
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_playlist_invalid"))]);

        expect(&fake, "https://api.spotify.com/v1/me/player/play?device_id=dev", reply(204, Value::Null));
        expect(&fake, "https://api.spotify.com/v1/me/player/repeat?state=off", reply(200, Value::Null));
        expect(&fake, "https://api.spotify.com/v1/me/player/shuffle?state=false", reply(200, Value::Null));
        w.handle(
            Input::Request(Request::Play {
                device: "dev".into(),
                uri: "spotify:track:0DiWol3AO6WpXZgp0goxAV".into(),
                position_ms: 5,
            }),
            now,
        );
        assert_eq!(w.take_updates(), [Update::Playing]);
        let log = fake.log.lock().unwrap();
        let play = log.iter().find(|r| r.1.contains("/play?")).unwrap();
        assert_eq!(play.3, Some(Body::Json(json!({"uris": ["spotify:track:0DiWol3AO6WpXZgp0goxAV"], "position_ms": 5}))));
    }

    #[test]
    fn sign_out_forgets_everything_and_keyring_failure_is_reported() {
        let now = Instant::now();
        let (mut w, fake, store) = worker(Memory { unavailable: true, ..Default::default() });
        w.handle(Input::Request(Request::SignIn { client_id: CLIENT.into() }), now);
        w.take_updates();
        let state = w.session.auth.expected_state().unwrap();
        expect(&fake, PROVIDER.token_url, token_reply("access", Some("refresh")));
        expect(&fake, "https://api.spotify.com/v1/me", reply(200, json!({"display_name": "M"})));
        w.handle(Input::Callback(format!("/callback?code=abc&state={state}")), now);
        let updates = w.take_updates();
        assert_eq!(updates[0], Update::Status(Message::new("spotify_keyring_unavailable")));
        assert!(matches!(updates[1], Update::SignedIn(_)));

        w.handle(Input::Request(Request::SignOut), now);
        assert_eq!(w.take_updates(), [Update::SignedOut, Update::Status(Message::new("spotify_disconnected"))]);
        assert!(!w.session.auth.signed_in() && store.load(CLIENT).is_none());
        w.handle(Input::Request(Request::Token), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_login_required")), Update::Token(None)]);
    }

    #[test]
    fn login_times_out_and_late_callback_is_ignored() {
        let now = Instant::now();
        let (mut w, fake, _) = worker(Memory::default());
        w.handle(Input::Request(Request::SignIn { client_id: CLIENT.into() }), now);
        w.take_updates();
        let state = w.session.auth.expected_state().unwrap();
        w.tick(now + crate::oauth::LOGIN_TIMEOUT);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_login_timeout"))]);
        assert!(w.session.auth.expected_state().is_none());
        w.handle(Input::Callback(format!("/callback?code=abc&state={state}")), now);
        assert!(w.take_updates().is_empty() && fake.log.lock().unwrap().is_empty());
        w.handle(Input::Request(Request::SignIn { client_id: "nope".into() }), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("spotify_client_invalid"))]);
    }

    /// Real sign-in, search, playlists and liked songs against Spotify:
    /// `FONO8_SPOTIFY_CLIENT_ID=... cargo test spotify_live -- --ignored --nocapture`.
    /// Restores the keyring session if there is one, otherwise opens the browser.
    #[test]
    #[ignore]
    fn spotify_live() {
        let client = std::env::var("FONO8_SPOTIFY_CLIENT_ID").expect("FONO8_SPOTIFY_CLIENT_ID");
        let spotify = Spotify::start(
            Box::new(crate::net::Https::new(super::super::HOSTS)),
            Box::new(crate::keystore::Keyring { service: "spotify" }),
            super::super::PORT,
            "Fono8: signed in, you can close this tab.".into(),
        );
        let next = |what: &str| loop {
            match spotify.wait(Duration::from_secs(240)) {
                Some(Update::OpenUrl(url)) => {
                    eprintln!("opening the browser for sign-in");
                    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                }
                Some(Update::Status(message)) => eprintln!("status: {}", message.key),
                Some(Update::Failed(message)) => panic!("{what}: {}", message.key),
                Some(update) => return update,
                None => panic!("{what}: timed out"),
            }
        };
        spotify.send(Request::Restore { client_id: client.clone() });
        let profile = match spotify.wait(Duration::from_secs(15)) {
            Some(Update::SignedIn(profile)) => profile,
            _ => {
                spotify.send(Request::SignIn { client_id: client.clone() });
                match next("sign-in") {
                    Update::SignedIn(profile) => profile,
                    other => panic!("sign-in: {other:?}"),
                }
            }
        };
        eprintln!("signed in as {:?}, premium {:?}", profile.name, profile.premium);
        spotify.send(Request::Search { query: "Daft Punk One More Time".into(), offset: 0 });
        let Update::Results { page, .. } = next("search") else { panic!() };
        eprintln!(
            "search: {} of {} (next {:?}), first {:?}",
            page.items.len(),
            page.total,
            page.next,
            page.items.first().map(|t| &t.title)
        );
        assert!(!page.items.is_empty() && page.items.len() <= 10);
        spotify.send(Request::Playlists { offset: 0 });
        let Update::Playlists(playlists) = next("playlists") else { panic!() };
        eprintln!("playlists: {} of {}", playlists.items.len(), playlists.total);
        eprintln!("importable: {}", playlists.items.iter().filter(|p| p.importable).count());
        if let Some(first) = playlists.items.iter().find(|p| p.importable) {
            spotify.send(Request::Import { playlist: first.id.clone(), enqueue: false });
            match spotify.wait(Duration::from_secs(60)) {
                Some(Update::Imported { name, tracks, .. }) => eprintln!("playlist {name:?}: {} tracks", tracks.len()),
                other => eprintln!("playlist {:?}: {other:?}", first.name),
            }
        }
        spotify.send(Request::Liked { enqueue: false });
        let Update::Imported { tracks, .. } = next("liked") else { panic!() };
        eprintln!("liked songs: {}", tracks.len());
        spotify.send(Request::Token);
        assert!(matches!(next("token"), Update::Token(Some(_))));
    }
}
