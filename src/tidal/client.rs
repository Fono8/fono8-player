//! The TIDAL worker: one background thread owns the session and all API traffic.
//! The UI sends [`Request`]s and drains [`Update`]s on its tick.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::i18n::Message;
use crate::keystore::TokenStore;
use crate::net::{Method, Response, Transport};
use crate::oauth::{CallError, Notice, Session};

use super::api::{self, Collection, CollectionKind, Item, Page, Profile};
use super::auth::PROVIDER;
use super::{parse_link, Link};

/// How many albums and playlists "My playlists" lists at most.
const MAX_COLLECTIONS: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    SignIn {
        client_id: String,
    },
    Restore {
        client_id: String,
    },
    /// All favorite tracks (bounded), imported as one list.
    Favorites {
        enqueue: bool,
    },
    /// Favorite albums and playlists.
    Collections,
    /// All tracks of an album or playlist (bounded).
    Open {
        kind: CollectionKind,
        id: String,
        enqueue: bool,
    },
    /// A pasted track, album or playlist link.
    OpenLink {
        link: String,
        enqueue: bool,
    },
    /// Download the 30-second preview of track `id` into `file`.
    Preview {
        id: String,
        file: std::path::PathBuf,
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
    /// A track opened from a link.
    Results {
        query: String,
        page: Page<Item>,
    },
    Collections(Vec<Collection>),
    Imported {
        name: String,
        items: Vec<Item>,
        enqueue: bool,
    },
    /// The preview of track `id` is in `file`, or could not be fetched.
    Preview {
        id: String,
        result: Result<std::path::PathBuf, Message>,
    },
}

enum Input {
    Request(Request),
    Callback(String),
    Quit,
}

pub struct Worker {
    session: Session,
    country: String,
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
        Worker { session, country: "US".into(), updates: Vec::new() }
    }

    fn emit(&mut self, update: Update) {
        self.updates.push(update);
    }

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
            Input::Request(request) => self.request(request, now),
            Input::Quit => {}
        }
        self.drain();
    }

    fn request(&mut self, request: Request, now: Instant) {
        match request {
            Request::SignIn { client_id } => {
                if let Some(url) = self.session.begin(&client_id, now) {
                    self.emit(Update::OpenUrl(url));
                    self.emit(Update::Status(Message::new("tidal_login_waiting")));
                }
            }
            Request::Restore { client_id } => {
                if self.session.restore(&client_id, now) {
                    self.drain();
                    self.signed_in(now);
                }
            }
            Request::Favorites { enqueue } => {
                if let Some(items) = self.collect(api::favorites_url(), now) {
                    self.emit(Update::Imported { name: String::new(), items, enqueue });
                }
            }
            Request::Collections => {
                let mut all = Vec::new();
                for url in [api::playlists_url(), api::albums_url()] {
                    let mut next = Some(url);
                    while let Some(url) = next.take() {
                        let Some(response) = self.call(&url, now) else { return };
                        let page = api::parse_collections(&response.body);
                        all.extend(page.items);
                        if all.len() < MAX_COLLECTIONS {
                            next = page.next;
                        }
                    }
                }
                all.truncate(MAX_COLLECTIONS);
                self.emit(Update::Collections(all));
            }
            Request::Open { kind, id, enqueue } => self.open(kind, &id, enqueue, now),
            Request::OpenLink { link, enqueue } => match parse_link(&link) {
                Some(Link::Album(id)) => self.open(CollectionKind::Album, &id, enqueue, now),
                Some(Link::Playlist(id)) => self.open(CollectionKind::Playlist, &id, enqueue, now),
                Some(Link::Track(id)) => {
                    if let Some(response) = self.call(&api::track_url(&id, &self.country), now) {
                        let items: Vec<Item> = api::parse_track(&response.body).into_iter().collect();
                        self.emit(Update::Results { query: link, page: Page { items, next: None } });
                    }
                }
                None => self.emit(Update::Failed(Message::new("tidal_link_invalid"))),
            },
            Request::Preview { id, file } => {
                let result = match self.call(&super::preview::manifest_url(&id), now) {
                    Some(response) => super::preview::manifest_uri(&response.body)
                        .and_then(|uri| super::preview::download(&uri, &file))
                        .map(|()| file)
                        .map_err(Message::new),
                    None => Err(Message::new("tidal_preview_unavailable")),
                };
                self.emit(Update::Preview { id, result });
            }
            Request::SignOut => {
                self.session.sign_out();
                self.emit(Update::SignedOut);
                self.emit(Update::Status(Message::new("tidal_disconnected")));
            }
        }
    }

    fn open(&mut self, kind: CollectionKind, id: &str, enqueue: bool, now: Instant) {
        let Some(response) = self.call(&api::collection_url(kind, id, &self.country), now) else { return };
        let name = api::parse_collection_name(&response.body);
        if let Some(items) = self.collect(api::collection_items_url(kind, id, &self.country), now) {
            let name = if name.is_empty() { id.to_string() } else { name };
            self.emit(Update::Imported { name, items, enqueue });
        }
    }

    fn signed_in(&mut self, now: Instant) {
        if let Some(response) = self.call(&api::me_url(), now) {
            let profile = api::parse_profile(&response.body);
            self.country = profile.country.clone();
            self.emit(Update::SignedIn(profile));
        }
    }

    /// An authorized GET; failures are reported and give `None`.
    fn call(&mut self, url: &str, now: Instant) -> Option<Response> {
        let result = self.session.call(Method::Get, url, None, now);
        self.drain();
        match result {
            Ok(response) => Some(response),
            Err(CallError::Http(response)) => {
                self.emit(Update::Failed(api::error(&response)));
                None
            }
            Err(CallError::Auth) => None,
        }
    }

    /// Follow `links.next` up to [`api::MAX_IMPORT`] tracks, without duplicates.
    fn collect(&mut self, url: String, now: Instant) -> Option<Vec<Item>> {
        let mut items: Vec<Item> = Vec::new();
        let mut next = Some(url);
        let mut pages = 0;
        while let Some(url) = next.take() {
            let response = self.call(&url, now)?;
            let page = api::parse_items(&response.body);
            for item in page.items {
                if !items.iter().any(|i| i.track.path == item.track.path) {
                    items.push(item);
                }
            }
            pages += 1;
            if items.len() < api::MAX_IMPORT && pages < 1000 {
                next = page.next;
            }
        }
        items.truncate(api::MAX_IMPORT);
        Some(items)
    }
}

/// Handle owned by the UI model.
pub struct Tidal {
    inbox: Sender<Input>,
    updates: Receiver<Update>,
    thread: Option<JoinHandle<()>>,
}

impl Tidal {
    pub fn start(transport: Box<dyn Transport>, store: Box<dyn TokenStore>, port: u16, done_text: String) -> Tidal {
        let (inbox, requests) = mpsc::channel::<Input>();
        let (updates_sender, updates) = mpsc::channel();
        let mut worker = Worker::new(transport, store, port, done_text, inbox.clone());
        let thread = std::thread::Builder::new()
            .name("fono8-tidal".into())
            .spawn(move || loop {
                let input = requests.recv_timeout(Duration::from_millis(500));
                let quit = matches!(input, Ok(Input::Quit) | Err(RecvTimeoutError::Disconnected));
                if let Ok(input) = input {
                    worker.handle(input, Instant::now());
                }
                worker.session.tick(Instant::now());
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
        Tidal { inbox, updates, thread }
    }

    pub fn send(&self, request: Request) {
        let _ = self.inbox.send(Input::Request(request));
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

impl Drop for Tidal {
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
    use crate::net::Body;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Default)]
    struct Fake {
        replies: Mutex<VecDeque<(&'static str, Response)>>,
        log: Mutex<Vec<(String, Option<String>)>>,
    }

    struct FakeTransport(Arc<Fake>);

    impl Transport for FakeTransport {
        fn request(&self, _method: Method, url: &str, bearer: Option<&str>, _body: Option<Body>) -> Response {
            self.0.log.lock().unwrap().push((url.to_string(), bearer.map(str::to_string)));
            let mut replies = self.0.replies.lock().unwrap();
            match replies.front() {
                Some((prefix, _)) if url.starts_with(prefix) => replies.pop_front().unwrap().1,
                other => panic!("unexpected request {url}, next reply is for {:?}", other.map(|r| r.0)),
            }
        }
    }

    const CLIENT: &str = "TestClientId0001";

    fn reply(status: u16, body: Value) -> Response {
        Response { status, body, retry_after: None }
    }

    fn worker() -> (Worker, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        let (inbox, _requests) = mpsc::channel();
        (Worker::new(Box::new(FakeTransport(fake.clone())), Box::new(Memory::default()), 0, "ok".into(), inbox), fake)
    }

    fn expect(fake: &Fake, prefix: &'static str, response: Response) {
        fake.replies.lock().unwrap().push_back((prefix, response));
    }

    fn track(id: &str) -> Value {
        json!({"id": id, "type": "tracks", "attributes": {"title": format!("T{id}"), "isrc": "GBDUW0700008", "duration": "PT1M"}})
    }

    #[test]
    fn sign_in_keeps_case_sensitive_client_id_and_sets_country() {
        let now = Instant::now();
        let (mut w, fake) = worker();
        w.handle(Input::Request(Request::SignIn { client_id: CLIENT.into() }), now);
        let updates = w.take_updates();
        let Some(Update::OpenUrl(url)) = updates.first() else { panic!("{updates:?}") };
        assert!(url.starts_with("https://login.tidal.com/authorize?") && url.contains("client_id=TestClientId0001"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A43821%2Ftidal%2Fcallback"));
        assert!(!url.contains("show_dialog"));
        let state = w.session.auth.expected_state().unwrap();
        expect(&fake, PROVIDER.token_url, reply(200, json!({"access_token": "a", "refresh_token": "r", "expires_in": 14400})));
        expect(
            &fake,
            "https://openapi.tidal.com/v2/users/me",
            reply(200, json!({"data": {"id": "7", "attributes": {"username": "m", "country": "PL"}}})),
        );
        w.handle(Input::Callback(format!("/tidal/callback?code=c&state={state}")), now);
        assert_eq!(w.take_updates(), [Update::SignedIn(Profile { id: "7".into(), name: "m".into(), country: "PL".into() })]);
        assert_eq!(w.country, "PL");
        assert_eq!(fake.log.lock().unwrap()[1].1.as_deref(), Some("a"));
        // Spotify's callback path is not TIDAL's.
        assert!(!w.session.callback(&format!("/callback?code=c&state={state}"), now));
    }

    #[test]
    fn favorites_follow_cursor_pages_and_links_open_collections() {
        let now = Instant::now();
        let (mut w, fake) = worker();
        w.session.auth.restore(CLIENT, "r".into());
        expect(&fake, PROVIDER.token_url, reply(200, json!({"access_token": "a", "expires_in": 14400})));
        expect(
            &fake,
            "https://openapi.tidal.com/v2/userCollectionTracks/me/relationships/items?include",
            reply(
                200,
                json!({"data": [{"id": "1", "type": "tracks"}], "included": [track("1")], "links": {"next": "/userCollectionTracks/me/relationships/items?page%5Bcursor%5D=2"}}),
            ),
        );
        expect(
            &fake,
            "https://openapi.tidal.com/v2/userCollectionTracks/me/relationships/items?page",
            reply(
                200,
                json!({"data": [{"id": "2", "type": "tracks"}, {"id": "1", "type": "tracks"}], "included": [track("2"), track("1")], "links": {}}),
            ),
        );
        w.handle(Input::Request(Request::Favorites { enqueue: true }), now);
        let updates = w.take_updates();
        let [Update::Imported { items, enqueue: true, .. }] = updates.as_slice() else { panic!("{updates:?}") };
        assert_eq!(items.iter().map(|i| i.track.path.as_str()).collect::<Vec<_>>(), ["tidal:track:1", "tidal:track:2"]);

        expect(
            &fake,
            "https://openapi.tidal.com/v2/albums/9?",
            reply(200, json!({"data": {"attributes": {"title": "Discovery"}}})),
        );
        expect(
            &fake,
            "https://openapi.tidal.com/v2/albums/9/relationships/items",
            reply(200, json!({"data": [{"id": "3", "type": "tracks"}], "included": [track("3")]})),
        );
        w.handle(Input::Request(Request::OpenLink { link: "https://tidal.com/browse/album/9".into(), enqueue: false }), now);
        let updates = w.take_updates();
        let [Update::Imported { name, items, enqueue: false }] = updates.as_slice() else { panic!("{updates:?}") };
        assert_eq!((name.as_str(), items.len()), ("Discovery", 1));

        w.handle(Input::Request(Request::OpenLink { link: "https://example.com/album/9".into(), enqueue: false }), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("tidal_link_invalid"))]);
        expect(&fake, "https://openapi.tidal.com/v2/tracks/5", reply(404, Value::Null));
        w.handle(Input::Request(Request::OpenLink { link: "https://tidal.com/browse/track/5".into(), enqueue: false }), now);
        assert_eq!(w.take_updates(), [Update::Failed(Message::new("tidal_not_found"))]);
    }

    /// Real sign-in, a track link, favorites and the collection against TIDAL:
    /// `FONO8_TIDAL_CLIENT_ID=... cargo test tidal_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn tidal_live() {
        let client = std::env::var("FONO8_TIDAL_CLIENT_ID").expect("FONO8_TIDAL_CLIENT_ID");
        let tidal = Tidal::start(
            Box::new(crate::net::Https::new(super::super::HOSTS).accept("application/vnd.api+json")),
            Box::new(crate::keystore::Keyring { service: "tidal" }),
            43821,
            "Fono8: sign-in received. You can close this tab and return to Fono8.".into(),
        );
        let next = |what: &str| loop {
            match tidal.wait(Duration::from_secs(240)) {
                Some(Update::OpenUrl(url)) => {
                    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                }
                Some(Update::Status(message)) => eprintln!("status: {}", message.key),
                Some(Update::Failed(message)) => panic!("{what}: {}", message.key),
                Some(update) => return update,
                None => panic!("{what}: timed out"),
            }
        };
        tidal.send(Request::Restore { client_id: client.clone() });
        let profile = match tidal.wait(Duration::from_secs(15)) {
            Some(Update::SignedIn(profile)) => profile,
            _ => {
                tidal.send(Request::SignIn { client_id: client });
                match next("sign-in") {
                    Update::SignedIn(profile) => profile,
                    other => panic!("{other:?}"),
                }
            }
        };
        eprintln!("signed in as {:?} ({})", profile.name, profile.country);
        tidal.send(Request::OpenLink { link: "https://tidal.com/browse/track/1198538".into(), enqueue: false });
        let Update::Results { page, .. } = next("track link") else { panic!() };
        eprintln!("track link: {:?}", page.items.first().map(|i| (&i.track.title, &i.track.artist, &i.isrc)));
        tidal.send(Request::Favorites { enqueue: false });
        let Update::Imported { items, .. } = next("favorites") else { panic!() };
        eprintln!("favorites: {}, with ISRC: {}", items.len(), items.iter().filter(|i| i.isrc.is_some()).count());
        let file = std::env::temp_dir().join(format!("fono8-tidal-preview-{}.mp4", std::process::id()));
        tidal.send(Request::Preview { id: "1198538".into(), file: file.clone() });
        match next("preview") {
            Update::Preview { result: Ok(path), .. } => {
                eprintln!("preview: {} bytes", std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
                let _ = std::fs::remove_file(path);
            }
            other => panic!("preview: {other:?}"),
        }
        tidal.send(Request::Collections);
        let Update::Collections(collections) = next("collections") else { panic!() };
        eprintln!("collections: {:?}", collections.iter().map(|c| (&c.name, c.tracks)).take(5).collect::<Vec<_>>());
    }
}
