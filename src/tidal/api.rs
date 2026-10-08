//! TIDAL API v2 (JSON:API) endpoints Fono8 uses and bounded parsers. Resources
//! come in `data` (or as identifiers in `data` with full resources in
//! `included`); pages continue through `links.next`. Remote JSON is untrusted:
//! ids are validated and text is length-limited.

use std::collections::HashMap;

use serde_json::Value;

use crate::i18n::Message;
use crate::library::Track;
use crate::net::Response;

use super::{is_id, is_playlist_id, track_path};

pub const API: &str = "https://openapi.tidal.com/v2";
const ORIGIN: &str = "https://openapi.tidal.com";
/// Upper bound for importing an album, a playlist or the favorites.
pub const MAX_IMPORT: usize = 10_000;
const MAX_TEXT: usize = 500;

/// A track with its ISRC, which identifies the recording on other services.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub track: Track,
    pub isrc: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionKind {
    Album,
    Playlist,
}

/// An album or playlist from the user's collection.
#[derive(Clone, Debug, PartialEq)]
pub struct Collection {
    pub kind: CollectionKind,
    pub id: String,
    pub name: String,
    /// Album artists, or empty for playlists.
    pub artist: String,
    pub tracks: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// The absolute URL of the next page.
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub country: String,
}

pub fn me_url() -> String {
    format!("{API}/users/me")
}

pub fn favorites_url() -> String {
    format!("{API}/userCollectionTracks/me/relationships/items?include=items,items.artists,items.albums")
}

pub fn albums_url() -> String {
    format!("{API}/userCollectionAlbums/me/relationships/items?include=items,items.artists")
}

pub fn playlists_url() -> String {
    format!("{API}/userCollectionPlaylists/me/relationships/items?include=items")
}

pub fn collection_url(kind: CollectionKind, id: &str, country: &str) -> String {
    match kind {
        CollectionKind::Album => format!("{API}/albums/{id}?countryCode={country}"),
        CollectionKind::Playlist => format!("{API}/playlists/{id}?countryCode={country}"),
    }
}

pub fn collection_items_url(kind: CollectionKind, id: &str, country: &str) -> String {
    let path = match kind {
        CollectionKind::Album => "albums",
        CollectionKind::Playlist => "playlists",
    };
    format!("{API}/{path}/{id}/relationships/items?countryCode={country}&include=items,items.artists,items.albums")
}

pub fn track_url(id: &str, country: &str) -> String {
    format!("{API}/tracks/{id}?countryCode={country}&include=artists,albums")
}

/// The absolute URL of a `links.next` value (TIDAL returns paths relative to the host or to `/v2`).
pub fn absolute(link: &str) -> Option<String> {
    if link.starts_with("https://openapi.tidal.com/") {
        Some(link.to_string())
    } else if link.starts_with("/v2/") {
        Some(format!("{ORIGIN}{link}"))
    } else if link.starts_with('/') {
        Some(format!("{API}{link}"))
    } else {
        None
    }
}

pub fn error(response: &Response) -> Message {
    Message::new(match response.status {
        0 => "tidal_network_error",
        401 => "tidal_auth_failed",
        403 => "tidal_access_denied",
        404 => "tidal_not_found",
        429 => "tidal_rate_limit",
        _ => "tidal_request_failed",
    })
}

fn text(value: Option<&Value>) -> String {
    let value = value.and_then(Value::as_str).unwrap_or("").trim();
    value.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

/// `PT1H14M46S` → seconds.
pub fn duration(value: &str) -> f64 {
    let Some(rest) = value.strip_prefix("PT") else { return 0.0 };
    let mut total = 0.0;
    let mut number = String::new();
    for c in rest.chars() {
        match c {
            '0'..='9' | '.' => number.push(c),
            'H' | 'M' | 'S' => {
                let n: f64 = number.parse().unwrap_or(0.0);
                total += n * match c {
                    'H' => 3600.0,
                    'M' => 60.0,
                    _ => 1.0,
                };
                number.clear();
            }
            _ => return 0.0,
        }
    }
    if total.is_finite() && total >= 0.0 {
        total
    } else {
        0.0
    }
}

/// `included` resources by (type, id).
fn index(body: &Value) -> HashMap<(String, String), &Value> {
    let mut map = HashMap::new();
    for resource in body.get("included").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().take(5000) {
        if let (Some(kind), Some(id)) = (resource.get("type").and_then(Value::as_str), resource.get("id").and_then(Value::as_str))
        {
            map.insert((kind.to_string(), id.to_string()), resource);
        }
    }
    map
}

fn related<'a>(resource: &'a Value, name: &str) -> Vec<&'a str> {
    resource
        .get("relationships")
        .and_then(|r| r.get(name))
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
        .map(|ids| ids.iter().take(20).filter_map(|i| i.get("id").and_then(Value::as_str)).collect())
        .unwrap_or_default()
}

fn names(resource: &Value, index: &HashMap<(String, String), &Value>, kind: &str, field: &str) -> Vec<String> {
    related(resource, kind)
        .into_iter()
        .filter_map(|id| index.get(&(kind.to_string(), id.to_string())))
        .map(|r| text(r.get("attributes").and_then(|a| a.get(field))))
        .filter(|n| !n.is_empty())
        .collect()
}

fn track(resource: &Value, index: &HashMap<(String, String), &Value>) -> Option<Item> {
    if resource.get("type").and_then(Value::as_str) != Some("tracks") {
        return None;
    }
    let id = resource.get("id").and_then(Value::as_str).filter(|id| is_id(id))?;
    let attributes = resource.get("attributes")?;
    let mut title = text(attributes.get("title"));
    if title.is_empty() {
        return None;
    }
    let version = text(attributes.get("version"));
    if !version.is_empty() && !title.contains(&version) {
        title = format!("{title} ({version})");
    }
    let artist = names(resource, index, "artists", "name").join(", ");
    let album = names(resource, index, "albums", "title").into_iter().next().unwrap_or_default();
    let duration = attributes.get("duration").and_then(Value::as_str).map(duration).unwrap_or(0.0);
    let isrc = attributes
        .get("isrc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|i| i.len() == 12 && i.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_ascii_uppercase);
    Some(Item { track: Track { path: track_path(id), title, artist, album, duration, ..Default::default() }, isrc })
}

fn next(links: Option<&Value>) -> Option<String> {
    links.and_then(|l| l.get("next")).and_then(Value::as_str).and_then(absolute)
}

/// Resources listed by identifier in `data` (a relationship page), resolved through `included`.
fn listed<'a>(body: &'a Value, index: &HashMap<(String, String), &'a Value>) -> Vec<&'a Value> {
    body.get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .take(MAX_IMPORT)
        .filter_map(|identifier| {
            let kind = identifier.get("type").and_then(Value::as_str)?;
            let id = identifier.get("id").and_then(Value::as_str)?;
            index.get(&(kind.to_string(), id.to_string())).copied()
        })
        .collect()
}

/// A page of tracks from a relationship (favorites, album or playlist items); videos are skipped.
pub fn parse_items(body: &Value) -> Page<Item> {
    let index = index(body);
    let items = listed(body, &index).into_iter().filter_map(|r| track(r, &index)).collect();
    Page { items, next: next(body.get("links")) }
}

/// One track (with its artists and album included).
pub fn parse_track(body: &Value) -> Option<Item> {
    track(body.get("data")?, &index(body))
}

/// Albums or playlists from the user's collection.
pub fn parse_collections(body: &Value) -> Page<Collection> {
    let index = index(body);
    let items = listed(body, &index)
        .into_iter()
        .filter_map(|resource| {
            let id = resource.get("id").and_then(Value::as_str)?;
            let attributes = resource.get("attributes")?;
            let tracks = attributes.get("numberOfItems").and_then(Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32;
            match resource.get("type").and_then(Value::as_str)? {
                "albums" if is_id(id) => Some(Collection {
                    kind: CollectionKind::Album,
                    id: id.to_string(),
                    name: text(attributes.get("title")),
                    artist: names(resource, &index, "artists", "name").join(", "),
                    tracks,
                }),
                "playlists" if is_playlist_id(id) => Some(Collection {
                    kind: CollectionKind::Playlist,
                    id: id.to_ascii_lowercase(),
                    name: text(attributes.get("name")),
                    artist: String::new(),
                    tracks,
                }),
                _ => None,
            }
        })
        .collect();
    Page { items, next: next(body.get("links")) }
}

/// The name of an album or playlist.
pub fn parse_collection_name(body: &Value) -> String {
    let attributes = body.get("data").and_then(|d| d.get("attributes"));
    let title = text(attributes.and_then(|a| a.get("title")));
    if title.is_empty() {
        text(attributes.and_then(|a| a.get("name")))
    } else {
        title
    }
}

pub fn parse_profile(body: &Value) -> Profile {
    let data = body.get("data").unwrap_or(&Value::Null);
    let attributes = data.get("attributes").unwrap_or(&Value::Null);
    let first = text(attributes.get("firstName"));
    let name = if first.is_empty() { text(attributes.get("username")) } else { first };
    let country = text(attributes.get("country"));
    let country = if country.len() == 2 && country.bytes().all(|b| b.is_ascii_alphabetic()) {
        country.to_ascii_uppercase()
    } else {
        "US".into()
    };
    Profile { id: text(data.get("id")), name, country }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track_resource(id: &str, title: &str, isrc: &str) -> Value {
        json!({"id": id, "type": "tracks", "attributes": {"title": title, "isrc": isrc, "duration": "PT3M56S", "version": ""},
            "relationships": {"artists": {"data": [{"id": "1", "type": "artists"}, {"id": "2", "type": "artists"}]},
                              "albums": {"data": [{"id": "9", "type": "albums"}]}}})
    }

    fn included() -> Vec<Value> {
        vec![
            json!({"id": "1", "type": "artists", "attributes": {"name": "Daft Punk"}}),
            json!({"id": "2", "type": "artists", "attributes": {"name": "Romanthony"}}),
            json!({"id": "9", "type": "albums", "attributes": {"title": "Discovery", "numberOfItems": 14}}),
        ]
    }

    #[test]
    fn durations() {
        assert_eq!(duration("PT3M56S"), 236.0);
        assert_eq!(duration("PT1H14M46S"), 4486.0);
        assert_eq!(duration("PT2.5S"), 2.5);
        assert_eq!(duration("garbage"), 0.0);
        assert_eq!(duration("PT3X"), 0.0);
    }

    #[test]
    fn parses_relationship_pages_with_included_resources() {
        let mut inc = included();
        inc.push(track_resource("1198538", "One More Time", "gbduw0700008"));
        inc.push(json!({"id": "77", "type": "videos", "attributes": {"title": "Video"}}));
        let body = json!({
            "data": [{"id": "1198538", "type": "tracks"}, {"id": "77", "type": "videos"}, {"id": "404", "type": "tracks"}],
            "included": inc,
            "links": {"self": "/x", "next": "/userCollectionTracks/me/relationships/items?page%5Bcursor%5D=abc"}
        });
        let page = parse_items(&body);
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        assert_eq!(item.track.path, "tidal:track:1198538");
        assert_eq!(
            (item.track.artist.as_str(), item.track.album.as_str(), item.track.duration),
            ("Daft Punk, Romanthony", "Discovery", 236.0)
        );
        assert_eq!(item.isrc.as_deref(), Some("GBDUW0700008"));
        assert_eq!(
            page.next.as_deref(),
            Some("https://openapi.tidal.com/v2/userCollectionTracks/me/relationships/items?page%5Bcursor%5D=abc")
        );
        assert_eq!(
            absolute("/v2/albums/1/relationships/items?page%5Bcursor%5D=x").as_deref(),
            Some("https://openapi.tidal.com/v2/albums/1/relationships/items?page%5Bcursor%5D=x")
        );
        assert_eq!(absolute("https://evil.test/x"), None);
    }

    #[test]
    fn parses_collections_and_profile() {
        let collections = parse_collections(&json!({
            "data": [{"id": "9", "type": "albums"}, {"id": "36ea71a8-445e-41a4-82ab-6628c581535d", "type": "playlists"}, {"id": "x", "type": "albums"}],
            "included": [
                {"id": "9", "type": "albums", "attributes": {"title": "Discovery", "numberOfItems": 14}, "relationships": {"artists": {"data": [{"id": "1", "type": "artists"}]}}},
                {"id": "1", "type": "artists", "attributes": {"name": "Daft Punk"}},
                {"id": "36ea71a8-445e-41a4-82ab-6628c581535d", "type": "playlists", "attributes": {"name": "Mix", "numberOfItems": 3}},
                {"id": "x", "type": "albums", "attributes": {"title": "Bad id"}}
            ]
        }));
        assert_eq!(collections.items.len(), 2);
        assert_eq!(
            (collections.items[0].name.as_str(), collections.items[0].artist.as_str(), collections.items[0].tracks),
            ("Discovery", "Daft Punk", 14)
        );
        assert_eq!(collections.items[1].kind, CollectionKind::Playlist);

        let profile = parse_profile(&json!({"data": {"id": "123", "attributes": {"username": "michal", "country": "pl"}}}));
        assert_eq!(profile, Profile { id: "123".into(), name: "michal".into(), country: "PL".into() });
        assert_eq!(parse_profile(&json!({})).country, "US");
        assert_eq!(parse_collection_name(&json!({"data": {"attributes": {"name": "Mix"}}})), "Mix");
    }
}
