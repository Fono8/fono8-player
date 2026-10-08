//! "Import from TIDAL": the TIDAL account card, the TIDAL tab of Discover and
//! the import itself, which matches every TIDAL track to a playable one
//! (YouTube Music by artist and title, then Spotify by ISRC) and saves the
//! matches as an ordinary Fono8 playlist. TIDAL is never a playback source.

use std::collections::HashSet;

use gpui::Context;
use serde_json::json;

use crate::i18n::{Arg, Message};
use crate::library::Track;
use crate::playback::Remote;
use crate::services::{AccountView, Button, Row, Service, ServiceAction};
use crate::spotify::client::Request as SpotifyRequest;
use crate::tidal::api::{Collection, CollectionKind, Item, Profile};
use crate::tidal::client::{Request as TidalRequest, Tidal, Update as TidalUpdate};
use crate::transfer::{pick_by_isrc, pick_by_name, Stage, Transfer, Wanted};
use crate::youtube::provider::Outcome;
use crate::youtube::Request;

use super::{debug, Dialog, Fono8};

/// A row on the TIDAL tab.
#[derive(Clone, Debug, PartialEq)]
pub enum TidalRow {
    /// All favorite tracks.
    Favorites,
    Collection(Collection),
    Track(Item),
}

/// Query prefix of the Spotify ISRC searches made while importing.
const ISRC: &str = "isrc:";

impl Fono8 {
    /// The TIDAL worker, started on first use.
    pub(super) fn tidal_client(&mut self) -> &Tidal {
        if self.tidal.is_none() {
            self.tidal = Some(Tidal::start(
                Box::new(crate::net::Https::new(crate::tidal::HOSTS).accept("application/vnd.api+json")),
                Box::new(crate::keystore::Keyring { service: "tidal" }),
                crate::spotify::PORT,
                self.t("tidal_signed_in_page"),
            ));
        }
        self.tidal.as_ref().unwrap()
    }

    pub(super) fn tidal_account_view(&self) -> AccountView {
        let t = |key: &str| self.t(key);
        let status = self.i18n.message(&self.tidal_account_message);
        match &self.tidal_profile {
            Some(profile) => AccountView {
                service: Service::Tidal,
                lines: vec![
                    self.text("tidal_signed_in_as", &[("name", Arg::Text(profile.name.clone()))]),
                    t("tidal_import_notice"),
                ],
                buttons: vec![
                    Button::new(ServiceAction::Account, "refresh", t("spotify_change_account")),
                    Button::new(ServiceAction::Disconnect, "trash", t("spotify_sign_out")),
                ],
                setup: None,
                status,
            },
            None => AccountView {
                service: Service::Tidal,
                lines: vec![t("tidal_status_signed_out"), t("tidal_notice")],
                buttons: Vec::new(),
                setup: self.tidal_client_input.clone().map(|input| {
                    (
                        input,
                        Button::new(ServiceAction::Setup, "tidal", t("tidal_sign_in")).accent(true),
                        self.text("tidal_redirect_hint", &[("uri", Arg::Text(crate::tidal::REDIRECT_URI.into()))]),
                    )
                }),
                status,
            },
        }
    }

    pub(super) fn tidal_account_action(&mut self, action: ServiceAction, cx: &mut Context<Self>) {
        match action {
            ServiceAction::Setup | ServiceAction::Account => {
                // Signing in uses the typed Client ID; changing the account keeps the saved one.
                let typed = self.tidal_client_input.as_ref().map(|i| i.read(cx).text().trim().to_string()).unwrap_or_default();
                let saved = self.library.setting_string("tidal_client_id").unwrap_or_default();
                let client_id = match action {
                    ServiceAction::Setup if !typed.is_empty() => typed,
                    _ if !saved.is_empty() => saved,
                    _ => typed,
                };
                if crate::tidal::auth::is_client_id(&client_id) {
                    self.library.set_setting("tidal_client_id", json!(client_id));
                }
                self.tidal_profile = None;
                self.tidal_rows.clear();
                self.discover.clear_service(Service::Tidal);
                self.tidal_account_message = Message::new("tidal_login_waiting");
                self.tidal_client().send(TidalRequest::SignIn { client_id });
            }
            ServiceAction::Disconnect => {
                self.tidal_profile = None;
                self.tidal_rows.clear();
                self.discover.clear_service(Service::Tidal);
                if let Some(tidal) = &self.tidal {
                    tidal.send(TidalRequest::SignOut);
                }
            }
        }
    }

    pub(super) fn tidal_request(&mut self, request: TidalRequest) {
        if self.tidal_profile.is_none() {
            self.tidal_message = Message::new("tidal_login_required");
            return;
        }
        self.tidal_busy = true;
        self.tidal_message = Message::new("tidal_loading");
        self.tidal_client().send(request);
    }

    /// "My albums and playlists": the favorites first, then the collection.
    pub(super) fn tidal_list_collections(&mut self) {
        self.tidal_request(TidalRequest::Collections);
    }

    pub(super) fn tidal_open_link(&mut self, link: String) {
        self.tidal_request(TidalRequest::OpenLink { link, enqueue: false });
    }

    pub(super) fn tidal_row(&self, row: &TidalRow, selected: bool) -> Row {
        let t = |key: &str| self.t(key);
        let make = |title: String, subtitle: String, artist: Option<String>| Row {
            service: Service::Tidal,
            title,
            subtitle,
            artist,
            cover: None,
            selected,
            enabled: true,
        };
        match row {
            TidalRow::Favorites => make(t("tidal_favorites"), t("tidal_favorites_hint"), None),
            TidalRow::Collection(collection) => {
                let kind = t(match collection.kind {
                    CollectionKind::Album => "album",
                    CollectionKind::Playlist => "playlist",
                });
                let count = self.text("tracks_count", &[("n", Arg::Count(collection.tracks as i64))]);
                let subtitle = if collection.artist.is_empty() {
                    format!("{kind} · {count}")
                } else {
                    format!("{kind} · {} · {count}", collection.artist)
                };
                make(collection.name.clone(), subtitle, None)
            }
            TidalRow::Track(item) => {
                make(item.track.title.clone(), self.i18n.artist(&item.track.artist), Some(item.track.artist.clone()))
            }
        }
    }

    /// Import (or enqueue) the selected TIDAL rows, one import at a time.
    pub(super) fn tidal_add(&mut self, indices: HashSet<usize>, enqueue: bool) {
        if self.transfer.is_some() || self.tidal_busy {
            self.tidal_message = Message::new("tidal_import_busy");
            return;
        }
        let mut indices: Vec<usize> = indices.into_iter().collect();
        indices.sort_unstable();
        let rows: Vec<TidalRow> = indices.into_iter().filter_map(|i| self.tidal_rows.get(i).cloned()).collect();
        let tracks: Vec<Item> =
            rows.iter().filter_map(|r| if let TidalRow::Track(item) = r { Some(item.clone()) } else { None }).collect();
        if !tracks.is_empty() {
            self.start_transfer(String::new(), tracks, enqueue);
            return;
        }
        match rows.as_slice() {
            [TidalRow::Favorites] => self.tidal_request(TidalRequest::Favorites { enqueue }),
            [TidalRow::Collection(collection)] => {
                self.tidal_request(TidalRequest::Open { kind: collection.kind, id: collection.id.clone(), enqueue })
            }
            [] => {}
            _ => self.tidal_message = Message::new("yt_select_one"),
        }
    }

    pub(super) fn poll_tidal(&mut self, cx: &mut Context<Self>) -> bool {
        let updates = self.tidal.as_ref().map(Tidal::poll).unwrap_or_default();
        let changed = !updates.is_empty();
        for update in updates {
            match update {
                TidalUpdate::OpenUrl(url) => cx.open_url(&url),
                TidalUpdate::Status(message) => {
                    self.tidal_message = message.clone();
                    self.tidal_account_message = message;
                }
                TidalUpdate::Failed(message) => {
                    debug(|| format!("tidal: {}", message.key));
                    self.tidal_busy = false;
                    self.tidal_message = message.clone();
                    if message.key.starts_with("tidal_login")
                        || matches!(
                            message.key,
                            "tidal_auth_failed" | "tidal_client_invalid" | "tidal_port_busy" | "tidal_session_expired"
                        )
                    {
                        self.tidal_account_message = message.clone();
                    }
                    self.set_status(message);
                }
                TidalUpdate::SignedIn(profile) => self.tidal_signed_in(profile),
                TidalUpdate::SignedOut => {
                    self.tidal_profile = None;
                    self.tidal_rows.clear();
                    self.discover.clear_service(Service::Tidal);
                    self.tidal_account_message = Message::new("tidal_status_signed_out");
                }
                TidalUpdate::Results { page, .. } => {
                    self.tidal_busy = false;
                    self.discover.clear_service(Service::Tidal);
                    self.tidal_rows = page.items.into_iter().map(TidalRow::Track).collect();
                    self.tidal_message = Message::new("tidal_link_track");
                }
                TidalUpdate::Collections(collections) => {
                    self.tidal_busy = false;
                    self.discover.clear_service(Service::Tidal);
                    self.tidal_rows =
                        std::iter::once(TidalRow::Favorites).chain(collections.into_iter().map(TidalRow::Collection)).collect();
                    self.tidal_message = Message::new("tidal_collections_listed");
                }
                TidalUpdate::Preview { id, result } => self.preview_arrived(&id, result),
                TidalUpdate::Imported { name, items, enqueue } => {
                    self.tidal_busy = false;
                    let name = if name.is_empty() { self.t("tidal_favorites_playlist") } else { name };
                    self.start_transfer(name, items, enqueue);
                }
            }
        }
        changed
    }

    fn tidal_signed_in(&mut self, profile: Profile) {
        debug(|| format!("tidal: signed in ({})", profile.country));
        self.tidal_account_message = Message::new("tidal_signed_in_as").with("name", profile.name.clone());
        self.tidal_message = Message::new("tidal_intro");
        self.tidal_profile = Some(profile);
    }

    // ----- import ---------------------------------------------------------

    /// How TIDAL lists are imported (saved setting).
    pub fn tidal_import_mode(&self) -> ImportMode {
        match self.library.setting_string("tidal_import_mode").as_deref() {
            Some("spotify") => ImportMode::SpotifyFirst,
            Some("preview") => ImportMode::Preview,
            _ => ImportMode::YouTubeFirst,
        }
    }

    pub fn set_tidal_import_mode(&mut self, mode: ImportMode) {
        let value = match mode {
            ImportMode::YouTubeFirst => "youtube",
            ImportMode::SpotifyFirst => "spotify",
            ImportMode::Preview => "preview",
        };
        self.library.set_setting("tidal_import_mode", json!(value));
    }

    /// Import `items`: as TIDAL previews, or matched to YouTube Music / Spotify.
    /// `name` empty adds them without creating a playlist.
    pub(super) fn start_transfer(&mut self, name: String, items: Vec<Item>, enqueue: bool) {
        if items.is_empty() {
            self.tidal_message = Message::new("tidal_import_empty");
            return;
        }
        let order = match self.tidal_import_mode() {
            ImportMode::Preview => {
                let tracks: Vec<Track> = items.into_iter().map(|item| item.track).collect();
                return self.save_import(&name, tracks, enqueue, Message::new("tidal_imported_previews").with("n", 0usize));
            }
            ImportMode::YouTubeFirst => vec![Remote::YouTube, Remote::Spotify],
            ImportMode::SpotifyFirst => vec![Remote::Spotify, Remote::YouTube],
        };
        let wanted = items.into_iter().map(|item| Wanted { track: item.track, isrc: item.isrc }).collect();
        self.transfer = Some(Transfer::new(name, wanted, enqueue, order));
        self.transfer_step();
    }

    fn transfer_progress(&mut self) {
        if let Some(transfer) = &self.transfer {
            if let Some(wanted) = transfer.current() {
                self.tidal_message = Message::new("tidal_matching")
                    .with("n", transfer.index + 1)
                    .with("total", transfer.wanted.len())
                    .with("title", format!("{} – {}", wanted.track.artist, wanted.track.title));
            }
        }
    }

    /// Send the next search, or finish. Called again on the tick while an engine is busy.
    pub(super) fn transfer_step(&mut self) {
        loop {
            let Some(transfer) = &mut self.transfer else { return };
            if transfer.done() {
                return self.finish_transfer();
            }
            if transfer.stage != Stage::Pending {
                return;
            }
            let Some(wanted) = transfer.current().cloned() else { return };
            let engine = transfer.next_engine();
            self.transfer_progress();
            match engine {
                None => {
                    if let Some(transfer) = &mut self.transfer {
                        transfer.resolve(None);
                    }
                }
                Some(Remote::YouTube) => {
                    if self.youtube.busy {
                        // Discover is using YouTube Music: try again on the next tick.
                        if let Some(transfer) = &mut self.transfer {
                            transfer.retry_engine();
                        }
                        return;
                    }
                    if self.transfer_try_youtube(&wanted) {
                        return;
                    }
                }
                Some(Remote::Spotify) => {
                    if self.transfer_try_spotify(&wanted) {
                        return;
                    }
                }
            }
        }
    }

    /// Search YouTube Music by artist and title; `false` when it cannot run.
    fn transfer_try_youtube(&mut self, wanted: &Wanted) -> bool {
        if !self.ensure_youtube() {
            return false;
        }
        let artist = crate::library::artist_parts(&wanted.track.artist).into_iter().next().map(|(a, _)| a).unwrap_or_default();
        if let Some(transfer) = &mut self.transfer {
            transfer.stage = Stage::YouTube;
        }
        self.youtube.matching = true;
        self.youtube_send(Request::Search(format!("{artist} {}", wanted.track.title).trim().to_string()));
        if self.youtube.busy {
            return true;
        }
        self.youtube.matching = false;
        if let Some(transfer) = &mut self.transfer {
            transfer.stage = Stage::Pending;
        }
        false
    }

    /// Search Spotify by ISRC; `false` when that is not possible.
    fn transfer_try_spotify(&mut self, wanted: &Wanted) -> bool {
        let (Some(isrc), Some(_)) = (&wanted.isrc, &self.spotify_profile) else { return false };
        let query = format!("{ISRC}{isrc}");
        if let Some(transfer) = &mut self.transfer {
            transfer.stage = Stage::Spotify;
        }
        self.spotify_client().send(SpotifyRequest::Search { query, offset: 0 });
        true
    }

    /// Record a result for the current track (`None`: try the next engine) and continue.
    fn transfer_found(&mut self, found: Option<(Track, Remote)>) {
        if let Some(transfer) = &mut self.transfer {
            match found {
                Some(found) => transfer.resolve(Some(found)),
                None => transfer.stage = Stage::Pending,
            }
        }
        self.transfer_step();
    }

    /// A YouTube Music outcome; `true` when it belonged to the import.
    pub(super) fn transfer_youtube(&mut self, outcome: &Outcome) -> bool {
        if !self.youtube.matching {
            return false;
        }
        self.youtube.matching = false;
        let Some(wanted) = self.transfer.as_ref().filter(|t| t.stage == Stage::YouTube).and_then(|t| t.current().cloned()) else {
            return true;
        };
        if let Outcome::Failed(message) = outcome {
            if matches!(message.key, "yt_open_account" | "yt_cancelled" | "yt_busy") {
                // The YouTube Music page is still loading: search again on a later tick.
                if let Some(transfer) = &mut self.transfer {
                    transfer.retry_engine();
                }
                return true;
            }
        }
        let found = match outcome {
            Outcome::Tracks(tracks) => pick_by_name(&wanted.track, tracks).map(|t| (t, Remote::YouTube)),
            _ => None,
        };
        self.transfer_found(found);
        true
    }

    /// Spotify search results or a failure while the import waits for Spotify; `true` when handled.
    pub(super) fn transfer_spotify(&mut self, query: Option<&str>, tracks: &[Track]) -> bool {
        let waiting = self.transfer.as_ref().is_some_and(|t| t.stage == Stage::Spotify);
        if !waiting || query.is_some_and(|q| !q.starts_with(ISRC)) {
            return false;
        }
        let Some(wanted) = self.transfer.as_ref().and_then(|t| t.current().cloned()) else { return false };
        self.transfer_found(pick_by_isrc(&wanted.track, tracks).map(|t| (t, Remote::Spotify)));
        true
    }

    fn finish_transfer(&mut self) {
        let Some(transfer) = self.transfer.take() else { return };
        let summary = Message::new("tidal_imported")
            .with("found", transfer.matched.len())
            .with("total", transfer.wanted.len())
            .with("youtube", transfer.from_youtube)
            .with("spotify", transfer.from_spotify);
        self.save_import(&transfer.name, transfer.matched, transfer.enqueue, summary);
        if !transfer.missing.is_empty() {
            let mut body = transfer.missing.iter().take(40).cloned().collect::<Vec<_>>().join("\n");
            if transfer.missing.len() > 40 {
                body.push_str(&format!("\n… +{}", transfer.missing.len() - 40));
            }
            self.dialog = Some(Dialog::Message {
                title: self.text("tidal_missing_title", &[("n", Arg::Count(transfer.missing.len() as i64))]),
                body,
            });
        }
    }

    /// Save imported tracks as a playlist (or into the Discover target when `name` is empty).
    fn save_import(&mut self, name: &str, tracks: Vec<Track>, enqueue: bool, summary: Message) {
        if !tracks.is_empty() {
            let saved = if name.is_empty() {
                self.library.add_remote_tracks(&tracks, self.discover.target).map(|_| None)
            } else {
                self.library.import_remote_playlist(name, &tracks).map(Some)
            };
            match saved {
                Ok(playlist) => {
                    self.refresh_playlists(playlist, playlist.is_none());
                    if enqueue {
                        self.enqueue_paths(tracks.iter().map(|t| t.path.clone()).collect());
                    }
                }
                Err(_) => {
                    self.tidal_message = Message::new("yt_save_error");
                    return;
                }
            }
        }
        // Previews: the count is only known here.
        let summary =
            if summary.key == "tidal_imported_previews" { Message::new(summary.key).with("n", tracks.len()) } else { summary };
        self.tidal_message = summary.clone();
        self.set_status(summary);
    }

    // ----- previews ---------------------------------------------------------

    /// Playback needs the 30-second preview of `path`: fetch it through the TIDAL session.
    pub(super) fn fetch_preview(&mut self, path: &str) {
        let Some(id) = crate::tidal::track_id(path).map(str::to_string) else { return };
        if self.tidal_profile.is_none() {
            self.set_status(Message::new("tidal_login_required"));
            return;
        }
        let Some(dir) = self.playback.preview_dir.clone() else { return };
        self.set_status(Message::new("tidal_preview_loading"));
        let file = crate::tidal::preview::cache_file(&dir, &id);
        self.tidal_client().send(TidalRequest::Preview { id, file });
    }

    fn preview_arrived(&mut self, id: &str, result: Result<std::path::PathBuf, Message>) {
        let current = self.playback.current().and_then(crate::tidal::track_id).is_some_and(|current| current == id);
        if !current {
            return;
        }
        match result {
            Ok(_) => self.playback.retry_current(),
            Err(message) => self.playback.preview_failed(message),
        }
    }
}

/// How a TIDAL list is imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// Matched on YouTube Music first, then on Spotify (by ISRC).
    YouTubeFirst,
    /// Matched on Spotify (by ISRC) first, then on YouTube Music.
    SpotifyFirst,
    /// Kept as TIDAL tracks that play TIDAL's 30-second preview.
    Preview,
}
