//! Spotify: PKCE sign-in with the user's own Client ID, a loopback callback and
//! player page, the Web API client and the refresh token in the system keyring
//! Audio plays through the official Web
//! Playback SDK in an installed Chrome/Edge, which brings Widevine.
//!
//! Spotify tracks live in the library as `spotify:track:<id>` paths.

pub mod api;
pub mod auth;
pub mod client;
pub mod panel;
pub mod player;

use url::Url;

pub const TRACK_PREFIX: &str = "spotify:track:";
/// The only hosts the Spotify client talks to.
pub static HOSTS: &[&str] = &["accounts.spotify.com", "api.spotify.com"];
pub const PORT: u16 = 43821;
pub const REDIRECT_URI: &str = "http://127.0.0.1:43821/callback";
pub const SCOPES: &str = "streaming user-read-email user-read-private user-modify-playback-state user-read-playback-state \
                          playlist-read-private playlist-read-collaborative user-library-read";

/// Spotify ids (tracks, playlists) are 22 base-62 characters.
pub fn is_id(value: &str) -> bool {
    value.len() == 22 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn is_track_path(path: &str) -> bool {
    path.strip_prefix(TRACK_PREFIX).is_some_and(is_id)
}

/// The track id of a `spotify:track:` path.
pub fn track_id(path: &str) -> Option<&str> {
    path.strip_prefix(TRACK_PREFIX).filter(|id| is_id(id))
}

pub fn track_path(id: &str) -> String {
    format!("{TRACK_PREFIX}{id}")
}

/// The open.spotify.com page Fono8 links to for attribution.
pub fn web_url(path: &str) -> Option<String> {
    track_id(path).map(|id| format!("https://open.spotify.com/track/{id}"))
}

/// The id from a `spotify:<kind>:<id>` URI or an `https://open.spotify.com/[intl-xx/]<kind>/<id>` link.
fn parse(value: &str, kind: &str) -> Result<String, ()> {
    let value = value.trim();
    if let Some(id) =
        value.strip_prefix("spotify:").and_then(|rest| rest.strip_prefix(kind)).and_then(|rest| rest.strip_prefix(':'))
    {
        return if is_id(id) { Ok(id.to_string()) } else { Err(()) };
    }
    let url = Url::parse(value).map_err(|_| ())?;
    if url.scheme() != "https"
        || url.host_str() != Some("open.spotify.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(());
    }
    let mut segments: Vec<&str> = url.path_segments().ok_or(())?.filter(|s| !s.is_empty()).collect();
    if segments.first().is_some_and(|s| s.len() == 7 && s.starts_with("intl-") && s[5..].bytes().all(|b| b.is_ascii_lowercase()))
    {
        segments.remove(0);
    }
    match segments.as_slice() {
        [k, id] if *k == kind && is_id(id) => Ok(id.to_string()),
        _ => Err(()),
    }
}

/// A track link or URI pasted by the user.
#[cfg(test)]
pub fn parse_track(value: &str) -> Result<String, ()> {
    parse(value, "track")
}

/// A playlist link or URI pasted by the user.
pub fn parse_playlist(value: &str) -> Result<String, ()> {
    parse(value, "playlist")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_links() {
        for value in [
            "spotify:track:4iV5W9uYEdYUVa79Axb7Rh",
            "https://open.spotify.com/track/4iV5W9uYEdYUVa79Axb7Rh?si=test",
            "https://open.spotify.com/intl-pl/track/4iV5W9uYEdYUVa79Axb7Rh",
            "  https://open.spotify.com/track/4iV5W9uYEdYUVa79Axb7Rh/  ",
        ] {
            assert_eq!(parse_track(value), Ok("4iV5W9uYEdYUVa79Axb7Rh".into()), "{value}");
        }
    }

    #[test]
    fn rejects_non_track_or_untrusted_links() {
        for value in [
            "https://open.spotify.com.evil.test/track/4iV5W9uYEdYUVa79Axb7Rh",
            "https://someone@open.spotify.com/track/4iV5W9uYEdYUVa79Axb7Rh",
            "https://open.spotify.com:8443/track/4iV5W9uYEdYUVa79Axb7Rh",
            "http://open.spotify.com/track/4iV5W9uYEdYUVa79Axb7Rh",
            "spotify:playlist:4iV5W9uYEdYUVa79Axb7Rh",
            "spotify:track:bad",
            "spotify:track:4iV5W9uYEdYUVa79Axb7R!",
            "file:///tmp/song",
            "",
        ] {
            assert!(parse_track(value).is_err(), "{value}");
        }
    }

    #[test]
    fn playlist_links_and_paths() {
        assert_eq!(
            parse_playlist("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=x"),
            Ok("37i9dQZF1DXcBWIGoYBM5M".into())
        );
        assert_eq!(parse_playlist("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"), Ok("37i9dQZF1DXcBWIGoYBM5M".into()));
        assert!(parse_playlist("https://open.spotify.com/album/37i9dQZF1DXcBWIGoYBM5M").is_err());
        let path = track_path("4iV5W9uYEdYUVa79Axb7Rh");
        assert!(is_track_path(&path));
        assert!(!is_track_path("ytmusic:dQw4w9WgXcQ"));
        assert_eq!(web_url(&path).as_deref(), Some("https://open.spotify.com/track/4iV5W9uYEdYUVa79Axb7Rh"));
    }
}
