//! Validated identifiers and parsers for the subset of YouTube Music responses
//! Fono8 uses, with tests.

use std::collections::HashMap;

use serde_json::Value;

use crate::library::{Track, REMOTE_PREFIX};

const MAX_NODES: usize = 200_000;

pub fn is_video_id(value: &str) -> bool {
    value.len() == 11 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The 11-character video id of a `ytmusic:` path or bare id.
pub fn video_id(value: &str) -> Result<String, ()> {
    let value = value.strip_prefix(REMOTE_PREFIX).unwrap_or(value);
    if is_video_id(value) {
        Ok(value.to_string())
    } else {
        Err(())
    }
}

/// A playlist id from a bare id, a `VL`-prefixed browse id or an HTTPS playlist link.
pub fn playlist_id(value: &str) -> Result<String, ()> {
    let mut value = value.trim().to_string();
    if value.contains("://") {
        let rest = value.strip_prefix("https://").ok_or(())?;
        let (authority, tail) = rest.split_once('/').map(|(a, t)| (a, format!("/{t}"))).unwrap_or((rest, "/".to_string()));
        if authority.contains('@') {
            return Err(());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() => (host, Some(port)),
            _ => (authority, None),
        };
        if !matches!(host, "music.youtube.com" | "www.youtube.com" | "youtube.com") || port.is_some_and(|p| p != "443") {
            return Err(());
        }
        let (path, query) =
            tail.split_once('?').map(|(p, q)| (p, q.split('#').next().unwrap_or(""))).unwrap_or((tail.as_str(), ""));
        if path != "/playlist" {
            return Err(());
        }
        value = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "list")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
    }
    if let Some(stripped) = value.strip_prefix("VL") {
        value = stripped.to_string();
    }
    let valid = (2..=160).contains(&value.len()) && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(value)
    } else {
        Err(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePlaylist {
    pub id: String,
    pub title: String,
}

/// Bounded, iterative traversal; remote JSON is untrusted input.
fn nodes<'a>(value: &'a Value, key: &str) -> Result<Vec<&'a Value>, ()> {
    let mut found = Vec::new();
    let mut pending = vec![value];
    let mut count = 0;
    while let Some(current) = pending.pop() {
        count += 1;
        if count > MAX_NODES {
            return Err(());
        }
        match current {
            Value::Object(map) => {
                if let Some(hit) = map.get(key) {
                    found.push(hit);
                }
                pending.extend(map.values().rev());
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            _ => {}
        }
    }
    Ok(found)
}

fn text(value: &Value) -> String {
    let Value::Object(map) = value else { return String::new() };
    if let Some(Value::String(simple)) = map.get("simpleText") {
        return simple.chars().take(2000).collect();
    }
    let runs = map.get("runs").and_then(Value::as_array);
    runs.map(|runs| runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)).collect::<String>())
        .unwrap_or_default()
        .chars()
        .take(2000)
        .collect()
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn parse_tracks(data: &Value) -> Result<Vec<Track>, ()> {
    let mut tracks = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for row in nodes(data, "musicResponsiveListItemRenderer")? {
        if !row.is_object() {
            continue;
        }
        if string(row, "musicItemRendererDisplayPolicy") == "MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT" {
            continue;
        }
        let mut candidate = row.get("playlistItemData").map(|p| string(p, "videoId")).unwrap_or("").to_string();
        if candidate.is_empty() {
            candidate = nodes(row, "watchEndpoint")?
                .iter()
                .map(|v| string(v, "videoId"))
                .find(|id| !id.is_empty())
                .unwrap_or("")
                .to_string();
        }
        if !is_video_id(&candidate) || seen.contains(&candidate) {
            continue;
        }
        let columns: Vec<&Value> = row
            .get("flexColumns")
            .and_then(Value::as_array)
            .map(|cols| {
                cols.iter()
                    .filter(|c| c.is_object())
                    .map(|c| {
                        c.get("musicResponsiveListItemFlexColumnRenderer").and_then(|r| r.get("text")).unwrap_or(&Value::Null)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let title = columns.first().map(|c| text(c)).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let mut artists: Vec<String> = Vec::new();
        let mut album = String::new();
        for column in columns.iter().skip(1) {
            for run in column.get("runs").and_then(Value::as_array).into_iter().flatten() {
                let browse_id = run
                    .get("navigationEndpoint")
                    .and_then(|n| n.get("browseEndpoint"))
                    .map(|b| string(b, "browseId"))
                    .unwrap_or("");
                if browse_id.starts_with("UC") {
                    artists.push(string(run, "text").to_string());
                } else if browse_id.starts_with("MPRE") || browse_id.starts_with("FEmusic_library_privately_owned_release") {
                    album = string(run, "text").to_string();
                }
            }
        }
        if artists.is_empty() && columns.len() > 1 {
            artists.push(text(columns[1]).split(" • ").next().unwrap_or("").to_string());
        }
        let mut seconds = 0.0;
        if let Some(fixed) = row.get("fixedColumns") {
            for value in nodes(fixed, "text")? {
                let duration = text(value);
                if let Some(parsed) = parse_duration(&duration) {
                    seconds = parsed as f64;
                    break;
                }
            }
        }
        tracks.push(Track {
            path: format!("{REMOTE_PREFIX}{candidate}"),
            title,
            artist: artists.join(", "),
            album,
            duration: seconds,
            sources: HashMap::new(),
        });
        seen.push(candidate);
    }
    Ok(tracks)
}

fn parse_duration(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    if parts[0].is_empty() || parts[0].len() > 3 || parts[1..].iter().any(|p| p.len() != 2) {
        return None;
    }
    let mut seconds = 0u64;
    for part in parts {
        seconds = seconds * 60 + part.parse::<u64>().ok()?;
    }
    Some(seconds)
}

pub fn parse_playlists(data: &Value) -> Result<Vec<RemotePlaylist>, ()> {
    let mut result: Vec<RemotePlaylist> = Vec::new();
    for kind in ["musicTwoRowItemRenderer", "musicResponsiveListItemRenderer"] {
        for row in nodes(data, kind)? {
            for browse in nodes(row, "browseEndpoint")? {
                let identifier = string(browse, "browseId");
                if !identifier.starts_with("VL") {
                    continue;
                }
                let mut title = row.get("title").map(text).unwrap_or_default();
                if title.is_empty() {
                    if let Some(first) = row.get("flexColumns").and_then(Value::as_array).and_then(|c| c.first()) {
                        title = nodes(first, "text")?.first().map(|t| text(t)).unwrap_or_default();
                    }
                }
                if !title.is_empty() {
                    let id = playlist_id(identifier)?;
                    if !result.iter().any(|p| p.id == id) {
                        result.push(RemotePlaylist { id, title });
                    }
                }
                break;
            }
        }
    }
    Ok(result)
}

/// Restrict parsing to the playlist shelf, excluding recommendations.
pub fn playlist_page(data: &Value) -> Result<&Value, ()> {
    for key in ["musicPlaylistShelfRenderer", "musicPlaylistShelfContinuation"] {
        if let Some(shelf) = nodes(data, key)?.first() {
            return Ok(shelf);
        }
    }
    for key in ["appendContinuationItemsAction", "reloadContinuationItemsCommand"] {
        if let Some(action) = nodes(data, key)?.first() {
            return Ok(action);
        }
    }
    Err(())
}

pub fn continuation(data: &Value) -> Result<Option<String>, ()> {
    for key in ["nextContinuationData", "continuationCommand"] {
        for item in nodes(data, key)? {
            let token = item.get("continuation").or_else(|| item.get("token")).and_then(Value::as_str).unwrap_or("");
            if !token.is_empty() && token.len() <= 16384 {
                return Ok(Some(token.to_string()));
            }
        }
    }
    Ok(None)
}

pub fn playlist_title(data: &Value, fallback: &str) -> Result<String, ()> {
    for kind in ["musicResponsiveHeaderRenderer", "musicDetailHeaderRenderer"] {
        for header in nodes(data, kind)? {
            let title = header.get("title").map(text).unwrap_or_default();
            if !title.is_empty() {
                return Ok(title);
            }
        }
    }
    Ok(fallback.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub const ONE: &str = "abcdefghijk";
    pub const TWO: &str = "lmnopqrstuv";

    pub fn row(identifier: &str, title: &str, grey: bool) -> Value {
        json!({
            "musicResponsiveListItemRenderer": {
                "playlistItemData": {"videoId": identifier},
                "musicItemRendererDisplayPolicy": if grey { "MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT" } else { "" },
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": "Artist", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCabc"}}},
                        {"text": " • "},
                        {"text": "Album", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREabc"}}}
                    ]}}}
                ],
                "fixedColumns": [
                    {"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "3:42"}]}}}
                ]
            }
        })
    }

    pub fn page(items: Vec<Value>, token: Option<&str>) -> Value {
        let mut shelf = json!({"contents": items});
        if let Some(token) = token {
            shelf["continuations"] = json!([{"nextContinuationData": {"continuation": token}}]);
        }
        json!({
            "header": {"musicResponsiveHeaderRenderer": {"title": {"runs": [{"text": "My mix"}]}}},
            "contents": {"musicPlaylistShelfRenderer": shelf},
            "recommendations": {"contents": [row("12345678901", "Do not import", false)]}
        })
    }

    #[test]
    fn playlist_inputs_cannot_select_arbitrary_network_destinations() {
        for value in [
            "https://evil.example/playlist?list=PLabc",
            "http://youtube.com/playlist?list=PLabc",
            "https://youtube.com.evil.example/playlist?list=PLabc",
            "file:///etc/passwd",
            "https://user@youtube.com/playlist?list=PLabc",
            "../private",
            "PLabc\nCookie: secret",
            "https://youtube.com:123/playlist?list=PLabc",
            "https://youtube.com/watch?list=PLabc",
        ] {
            assert!(playlist_id(value).is_err(), "{value}");
        }
    }

    #[test]
    fn normalized_identifiers_and_parser_excludes_unavailable_duplicate_and_recommendations() {
        assert_eq!(playlist_id("https://music.youtube.com/playlist?list=PLabc&si=anything").unwrap(), "PLabc");
        assert_eq!(playlist_id("VLPLabc").unwrap(), "PLabc");
        assert_eq!(video_id(&format!("ytmusic:{ONE}")).unwrap(), ONE);
        assert!(video_id("';alert(1)//").is_err());
        let data = page(vec![row(ONE, "Song", false), row(ONE, "Song", false), row(TWO, "Song", true)], None);
        let tracks = parse_tracks(playlist_page(&data).unwrap()).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].path, format!("ytmusic:{ONE}"));
        assert_eq!(
            (tracks[0].title.as_str(), tracks[0].artist.as_str(), tracks[0].album.as_str(), tracks[0].duration),
            ("Song", "Artist", "Album", 222.0)
        );
        assert_eq!(tracks[0].external_path(), format!("https://music.youtube.com/watch?v={ONE}"));
    }

    #[test]
    fn search_without_playlist_item_data_uses_watch_endpoint() {
        let mut item = row(ONE, "Song", false)["musicResponsiveListItemRenderer"].clone();
        item.as_object_mut().unwrap().remove("playlistItemData");
        item["overlay"] = json!({"watchEndpoint": {"videoId": ONE}});
        let tracks = parse_tracks(&json!({"musicResponsiveListItemRenderer": item})).unwrap();
        assert_eq!(tracks[0].path, format!("ytmusic:{ONE}"));
    }

    #[test]
    fn playlist_parser_ignores_album_navigation() {
        let data = json!({"items": [
            {"musicTwoRowItemRenderer": {"title": {"runs": [{"text": "List"}]}, "navigationEndpoint": {"browseEndpoint": {"browseId": "VLPLabc"}}}},
            {"musicTwoRowItemRenderer": {"title": {"simpleText": "Album"}, "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREabc"}}}}
        ]});
        let playlists = parse_playlists(&data).unwrap();
        assert_eq!(playlists, vec![RemotePlaylist { id: "PLabc".into(), title: "List".into() }]);
    }

    #[test]
    fn continuation_and_title() {
        let data = page(vec![row(ONE, "Song", false)], Some("page2"));
        assert_eq!(continuation(playlist_page(&data).unwrap()).unwrap().as_deref(), Some("page2"));
        assert_eq!(playlist_title(&data, "fallback").unwrap(), "My mix");
        assert!(playlist_page(&json!({"unknown": "new schema"})).is_err());
    }
}
