//! Web API endpoints Fono8 uses and bounded parsers for their replies. Spotify
//! JSON is untrusted input: ids are validated and text is length-limited.
//!
//! Development-mode limits (2026): search returns at most 10 items per page and
//! playlist contents are only available for playlists the user owns or
//! collaborates on.

use serde_json::Value;

use crate::i18n::Message;
use crate::library::Track;

use super::{is_id, track_path};
use crate::net::Response;

pub const API: &str = "https://api.spotify.com/v1";
pub const SEARCH_PAGE: u32 = 10;
pub const PAGE: u32 = 50;
/// Upper bound for importing a playlist or the liked songs.
pub const MAX_IMPORT: usize = 10_000;
const MAX_TEXT: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// `Some(true)` for Premium; `None` when Spotify no longer reports it.
    pub premium: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub tracks: u32,
    /// Owned or collaborative: only these expose their tracks to development-mode apps.
    pub importable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u32,
    /// The offset of the next page, if there is one.
    pub next: Option<u32>,
}

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

pub fn me_url() -> String {
    format!("{API}/me")
}

pub fn search_url(query: &str, offset: u32) -> String {
    format!("{API}/search?type=track&limit={SEARCH_PAGE}&offset={offset}&q={}", encode(query))
}

pub fn playlists_url(offset: u32) -> String {
    format!("{API}/me/playlists?limit={PAGE}&offset={offset}")
}

pub fn playlist_url(id: &str) -> String {
    format!("{API}/playlists/{id}?fields=id,name,owner(display_name,id)")
}

pub fn playlist_items_url(id: &str, offset: u32) -> String {
    format!("{API}/playlists/{id}/items?limit={PAGE}&offset={offset}&additional_types=track")
}

pub fn saved_tracks_url(offset: u32) -> String {
    format!("{API}/me/tracks?limit={PAGE}&offset={offset}")
}

pub fn play_url(device: &str) -> String {
    format!("{API}/me/player/play?device_id={}", encode(device))
}

pub fn repeat_off_url(device: &str) -> String {
    format!("{API}/me/player/repeat?state=off&device_id={}", encode(device))
}

pub fn shuffle_off_url(device: &str) -> String {
    format!("{API}/me/player/shuffle?state=false&device_id={}", encode(device))
}

/// The error message for a failed API reply.
pub fn error(response: &Response) -> Message {
    Message::new(match response.status {
        0 => "spotify_network_error",
        401 => "spotify_auth_failed",
        403 => "spotify_access_denied",
        404 => "spotify_not_found",
        429 => "spotify_rate_limit",
        _ => "spotify_request_failed",
    })
}

fn text(value: Option<&Value>) -> String {
    let value = value.and_then(Value::as_str).unwrap_or("").trim();
    value.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

fn count(value: Option<&Value>) -> u32 {
    value.and_then(Value::as_u64).map(|n| n.min(u32::MAX as u64) as u32).unwrap_or(0)
}

pub fn parse_profile(body: &Value) -> Profile {
    let id = text(body.get("id"));
    let name = text(body.get("display_name"));
    let name = if name.is_empty() { id.clone() } else { name };
    Profile { id, name, premium: body.get("product").and_then(Value::as_str).map(|p| p == "premium") }
}

/// A playable catalogue track; local files, episodes and unavailable items are skipped.
pub fn parse_track(value: &Value) -> Option<Track> {
    if value.get("type").and_then(Value::as_str) != Some("track") || value.get("is_local").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let id = value.get("id").and_then(Value::as_str).filter(|id| is_id(id))?;
    let title = text(value.get("name"));
    if title.is_empty() {
        return None;
    }
    let artists: Vec<String> = value
        .get("artists")
        .and_then(Value::as_array)
        .map(|artists| artists.iter().take(20).map(|a| text(a.get("name"))).filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    let mut artist = artists.join(", ");
    if artist.chars().count() > MAX_TEXT {
        artist = artist.chars().take(MAX_TEXT).collect();
    }
    let album = text(value.get("album").and_then(|a| a.get("name")));
    let duration = value.get("duration_ms").and_then(Value::as_f64).filter(|d| d.is_finite() && *d > 0.0).unwrap_or(0.0) / 1000.0;
    Some(Track { path: track_path(id), title, artist, album, duration, ..Default::default() })
}

fn page<T>(body: &Value, container: Option<&str>, item: impl Fn(&Value) -> Option<T>) -> Page<T> {
    let body = match container {
        Some(key) => body.get(key).unwrap_or(&Value::Null),
        None => body,
    };
    let entries = body.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let items = entries.iter().take(PAGE as usize).filter_map(item).collect();
    let total = count(body.get("total"));
    let offset = count(body.get("offset"));
    let limit = count(body.get("limit")).max(entries.len() as u32);
    let has_next = body.get("next").is_some_and(|n| n.is_string()) && limit > 0;
    let next = offset.checked_add(limit).filter(|next| has_next && *next < total.max(offset + 1));
    Page { items, total, next }
}

pub fn parse_search(body: &Value) -> Page<Track> {
    page(body, Some("tracks"), parse_track)
}

/// Saved tracks and playlist items wrap the track: `{"track": ...}` or, since
/// the February 2026 API changes, `{"item": ...}`.
pub fn parse_wrapped_tracks(body: &Value) -> Page<Track> {
    page(body, None, |entry| entry.get("item").or_else(|| entry.get("track")).and_then(parse_track))
}

/// The user's playlists; `user` is the signed-in user's id.
pub fn parse_playlists(body: &Value, user: &str) -> Page<Playlist> {
    page(body, None, |value| {
        let id = value.get("id").and_then(Value::as_str).filter(|id| is_id(id))?;
        let name = text(value.get("name"));
        let owner = text(value.get("owner").and_then(|o| o.get("display_name")));
        let owned = !user.is_empty() && value.get("owner").and_then(|o| o.get("id")).and_then(Value::as_str) == Some(user);
        let importable = owned || value.get("collaborative").and_then(Value::as_bool) == Some(true);
        let tracks = value.get("items").or_else(|| value.get("tracks")).map(|t| count(t.get("total"))).unwrap_or(0);
        Some(Playlist { id: id.to_string(), name, owner, tracks, importable })
    })
}

pub fn parse_playlist_name(body: &Value) -> String {
    text(body.get("name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track(id: &str, name: &str) -> Value {
        json!({
            "type": "track", "id": id, "name": name, "duration_ms": 320357,
            "artists": [{"name": "Daft Punk"}, {"name": "Romanthony"}],
            "album": {"name": "Discovery"}
        })
    }

    #[test]
    fn parses_search_with_paging_and_skips_bad_items() {
        let body = json!({"tracks": {
            "items": [
                track("0DiWol3AO6WpXZgp0goxAV", "One More Time"),
                track("bad", "Wrong id"),
                {"type": "episode", "id": "0DiWol3AO6WpXZgp0goxAW", "name": "Podcast"},
                track("0DiWol3AO6WpXZgp0goxAX", "  \u{7}  "),
                {"type": "track", "id": "0DiWol3AO6WpXZgp0goxAY", "name": "Local", "is_local": true}
            ],
            "total": 25, "offset": 10, "limit": 10, "next": "https://api.spotify.com/v1/search?offset=20"
        }});
        let page = parse_search(&body);
        assert_eq!(page.items.len(), 1);
        let t = &page.items[0];
        assert_eq!(t.path, "spotify:track:0DiWol3AO6WpXZgp0goxAV");
        assert_eq!(
            (t.title.as_str(), t.artist.as_str(), t.album.as_str()),
            ("One More Time", "Daft Punk, Romanthony", "Discovery")
        );
        assert!((t.duration - 320.357).abs() < 1e-9);
        assert_eq!((page.total, page.next), (25, Some(20)));
        let last = json!({"tracks": {"items": [], "total": 25, "offset": 20, "limit": 10, "next": null}});
        assert_eq!(parse_search(&last).next, None);
        assert_eq!(parse_search(&json!({"error": {"status": 400}})), Page { items: vec![], total: 0, next: None });
    }

    #[test]
    fn parses_old_and_new_playlist_item_shapes_and_playlists() {
        let body = json!({"items": [
            {"item": track("0DiWol3AO6WpXZgp0goxAV", "New shape")},
            {"track": track("0DiWol3AO6WpXZgp0goxAW", "Old shape")},
            {"track": null}
        ], "total": 3, "offset": 0, "limit": 50, "next": null});
        let titles: Vec<_> = parse_wrapped_tracks(&body).items.into_iter().map(|t| t.title).collect();
        assert_eq!(titles, ["New shape", "Old shape"]);

        let playlists = parse_playlists(
            &json!({"items": [
            {"id": "37i9dQZF1DXcBWIGoYBM5M", "name": "Mine", "owner": {"display_name": "Michał", "id": "me"}, "items": {"total": 12}},
            {"id": "37i9dQZF1DXcBWIGoYBM5N", "name": "Old", "owner": {"id": "other"}, "tracks": {"total": 3}, "collaborative": true},
            {"id": "37i9dQZF1DXcBWIGoYBM5O", "name": "Followed", "owner": {"id": "other"}},
            {"id": "nope", "name": "Bad"}
        ], "total": 4, "offset": 0, "limit": 50, "next": null}),
            "me",
        );
        assert_eq!(
            playlists.items,
            [
                Playlist {
                    id: "37i9dQZF1DXcBWIGoYBM5M".into(),
                    name: "Mine".into(),
                    owner: "Michał".into(),
                    tracks: 12,
                    importable: true
                },
                Playlist {
                    id: "37i9dQZF1DXcBWIGoYBM5N".into(),
                    name: "Old".into(),
                    owner: String::new(),
                    tracks: 3,
                    importable: true
                },
                Playlist {
                    id: "37i9dQZF1DXcBWIGoYBM5O".into(),
                    name: "Followed".into(),
                    owner: String::new(),
                    tracks: 0,
                    importable: false
                },
            ]
        );
    }

    #[test]
    fn urls_encode_parameters_and_profile_reports_premium() {
        assert_eq!(
            search_url("Daft Punk & co", 10),
            "https://api.spotify.com/v1/search?type=track&limit=10&offset=10&q=Daft+Punk+%26+co"
        );
        assert_eq!(play_url("a b&c"), "https://api.spotify.com/v1/me/player/play?device_id=a+b%26c");
        assert_eq!(parse_profile(&json!({"display_name": "M", "product": "premium"})).premium, Some(true));
        assert_eq!(
            parse_profile(&json!({"id": "user", "product": "free"})),
            Profile { id: "user".into(), name: "user".into(), premium: Some(false) }
        );
        assert_eq!(parse_profile(&json!({"display_name": "M"})).premium, None);
        let status = |status| error(&Response { status, body: Value::Null, retry_after: None }).key;
        assert_eq!(
            [status(0), status(401), status(403), status(429), status(500)],
            [
                "spotify_network_error",
                "spotify_auth_failed",
                "spotify_access_denied",
                "spotify_rate_limit",
                "spotify_request_failed"
            ]
        );
    }
}
