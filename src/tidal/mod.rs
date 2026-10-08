//! TIDAL through its official API (openapi.tidal.com v2), as an import source:
//! PKCE sign-in with the user's own Client ID, favorites, albums, playlists and
//! links. Third-party apps cannot stream full TIDAL tracks, so the user picks
//! how to import: matched to YouTube Music / Spotify (see `transfer.rs`), or as
//! TIDAL tracks (`tidal:track:<id>` paths) that play TIDAL's 30-second preview.

pub mod api;
pub mod auth;
pub mod client;
pub mod preview;

use url::Url;

pub const TRACK_PREFIX: &str = "tidal:track:";
pub const REDIRECT_URI: &str = "http://127.0.0.1:43821/tidal/callback";
pub const SCOPES: &str = "user.read collection.read playlists.read search.read";
/// The only hosts the TIDAL client talks to.
pub static HOSTS: &[&str] = &["auth.tidal.com", "openapi.tidal.com"];

/// Track and album ids are numeric.
pub fn is_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Playlist ids are UUIDs.
pub fn is_playlist_id(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn track_id(path: &str) -> Option<&str> {
    path.strip_prefix(TRACK_PREFIX).filter(|id| is_id(id))
}

pub fn is_track_path(path: &str) -> bool {
    track_id(path).is_some()
}

pub fn web_url(path: &str) -> Option<String> {
    track_id(path).map(|id| format!("https://tidal.com/browse/track/{id}"))
}

pub fn track_path(id: &str) -> String {
    format!("{TRACK_PREFIX}{id}")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Track(String),
    Album(String),
    Playlist(String),
}

/// A pasted `tidal.com` / `listen.tidal.com` link (with or without `/browse`) or `tidal:track:` path.
pub fn parse_link(value: &str) -> Option<Link> {
    let value = value.trim();
    if let Some(id) = track_id(value) {
        return Some(Link::Track(id.to_string()));
    }
    let url = Url::parse(value).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https"
        || !matches!(host, "tidal.com" | "www.tidal.com" | "listen.tidal.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segments.first() == Some(&"browse") {
        segments.remove(0);
    }
    match segments.as_slice() {
        ["track", id] | ["album", _, "track", id] if is_id(id) => Some(Link::Track(id.to_string())),
        ["album", id] if is_id(id) => Some(Link::Album(id.to_string())),
        ["playlist", id] if is_playlist_id(id) => Some(Link::Playlist(id.to_ascii_lowercase())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_paths() {
        assert_eq!(parse_link("https://tidal.com/browse/track/1198538"), Some(Link::Track("1198538".into())));
        assert_eq!(parse_link("https://listen.tidal.com/album/1198530"), Some(Link::Album("1198530".into())));
        assert_eq!(parse_link("https://tidal.com/album/1198530/track/1198538?u"), Some(Link::Track("1198538".into())));
        assert_eq!(
            parse_link("https://tidal.com/browse/playlist/36EA71A8-445E-41A4-82AB-6628C581535D"),
            Some(Link::Playlist("36ea71a8-445e-41a4-82ab-6628c581535d".into()))
        );
        assert_eq!(parse_link("tidal:track:42"), Some(Link::Track("42".into())));
        for bad in [
            "http://tidal.com/browse/track/1",
            "https://tidal.com.evil.test/track/1",
            "https://user@tidal.com/track/1",
            "https://tidal.com/track/abc",
            "https://tidal.com/playlist/not-a-uuid",
            "https://tidal.com/artist/123",
            "",
        ] {
            assert_eq!(parse_link(bad), None, "{bad}");
        }
        assert_eq!(track_id(&track_path("1198538")), Some("1198538"));
        assert_eq!(track_id("tidal:track:x"), None);
    }
}
