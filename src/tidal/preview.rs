//! TIDAL's 30-second previews: the official track manifest (`/trackManifests`)
//! serves third-party apps an unencrypted HLS preview (fragmented MP4 with
//! AAC-LC). The init segment and the media segments are joined into one MP4
//! file that Fono8's own decoder plays like a local file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use url::Url;

use super::api::API;

/// A preview is about 1.2 MB; anything much larger is not a preview.
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SEGMENTS: usize = 64;
/// How many cached previews to keep.
const MAX_CACHED: usize = 300;

pub fn manifest_url(id: &str) -> String {
    format!("{API}/trackManifests/{id}?manifestType=HLS&formats=AACLC&uriScheme=HTTPS&usage=PLAYBACK&adaptive=false")
}

/// Only TIDAL's manifest and audio hosts over HTTPS.
pub fn allowed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.port().is_none()
            && url.host_str().is_some_and(|h| h.ends_with(".manifest.tidal.com") || h.ends_with(".audio.tidal.com"))
    })
}

/// The manifest URI, if the manifest is a playable preview without DRM.
pub fn manifest_uri(body: &Value) -> Result<String, &'static str> {
    let attributes = body.get("data").and_then(|d| d.get("attributes")).ok_or("tidal_preview_unavailable")?;
    if attributes.get("drmData").is_some_and(|d| !d.is_null()) {
        return Err("tidal_preview_unavailable");
    }
    attributes.get("uri").and_then(Value::as_str).filter(|u| allowed(u)).map(str::to_string).ok_or("tidal_preview_unavailable")
}

/// The first variant of a master playlist, or the segments (init first) of a media playlist.
pub enum Playlist {
    Master(String),
    Media(Vec<String>),
}

pub fn parse_playlist(text: &str, base: &str) -> Option<Playlist> {
    let base = Url::parse(base).ok()?;
    let resolve = |reference: &str| base.join(reference.trim()).ok().map(|u| u.to_string()).filter(|u| allowed(u));
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if lines.first() != Some(&"#EXTM3U") {
        return None;
    }
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let variant = lines.iter().find(|l| !l.is_empty() && !l.starts_with('#'))?;
        return resolve(variant).map(Playlist::Master);
    }
    let mut segments = Vec::new();
    if let Some(map) = lines.iter().find(|l| l.starts_with("#EXT-X-MAP:")) {
        let uri = map.split("URI=\"").nth(1)?.split('"').next()?;
        segments.push(resolve(uri)?);
    }
    for line in lines.iter().filter(|l| !l.is_empty() && !l.starts_with('#')).take(MAX_SEGMENTS) {
        segments.push(resolve(line)?);
    }
    (!segments.is_empty()).then_some(Playlist::Media(segments))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).max_redirects(0).build().new_agent()
}

fn fetch(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, &'static str> {
    if !allowed(url) {
        return Err("tidal_preview_unavailable");
    }
    let mut response = agent.get(url).call().map_err(|_| "tidal_network_error")?;
    response.body_mut().with_config().limit(limit).read_to_vec().map_err(|_| "tidal_network_error")
}

/// Download the preview behind `manifest_uri` into `file`.
pub fn download(manifest_uri: &str, file: &Path) -> Result<(), &'static str> {
    let agent = agent();
    let mut url = manifest_uri.to_string();
    let segments = loop {
        let text = String::from_utf8(fetch(&agent, &url, 256 * 1024)?).map_err(|_| "tidal_preview_unavailable")?;
        match parse_playlist(&text, &url).ok_or("tidal_preview_unavailable")? {
            Playlist::Master(variant) if variant != url => url = variant,
            Playlist::Master(_) => return Err("tidal_preview_unavailable"),
            Playlist::Media(segments) => break segments,
        }
    };
    let mut data = Vec::new();
    for segment in segments {
        let left = MAX_BYTES.saturating_sub(data.len() as u64);
        data.extend(fetch(&agent, &segment, left)?);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|_| "tidal_preview_unavailable")?;
        prune(dir);
    }
    let partial = file.with_extension("part");
    std::fs::write(&partial, &data).and_then(|_| std::fs::rename(&partial, file)).map_err(|_| "tidal_preview_unavailable")
}

/// The cached preview of track `id`.
pub fn cache_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.mp4"))
}

/// Keep the cache below [`MAX_CACHED`] files, dropping the oldest.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(_, p)| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    if files.len() < MAX_CACHED {
        return;
    }
    files.sort();
    for (_, path) in files.iter().take(files.len() + 1 - MAX_CACHED) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_drm_free_previews_on_tidal_hosts() {
        let ok = json!({"data": {"attributes": {"uri": "https://im-fa.manifest.tidal.com/1/m.m3u8?token=x", "trackPresentation": "PREVIEW"}}});
        assert_eq!(manifest_uri(&ok).as_deref(), Ok("https://im-fa.manifest.tidal.com/1/m.m3u8?token=x"));
        let drm = json!({"data": {"attributes": {"uri": "https://im-fa.manifest.tidal.com/1/m.m3u8", "drmData": {"x": 1}}}});
        assert!(manifest_uri(&drm).is_err());
        let elsewhere = json!({"data": {"attributes": {"uri": "https://evil.test/m.m3u8"}}});
        assert!(manifest_uri(&elsewhere).is_err());
        assert!(!allowed("http://sp-ad-fa.audio.tidal.com/x") && !allowed("https://audio.tidal.com.evil.test/x"));
    }

    #[test]
    fn parses_master_and_media_playlists() {
        let master = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=321858,CODECS=\"mp4a.40.2\"\nhttps://im-fa.manifest.tidal.com/1/manifests/a.m3u8?token=t\n";
        let Some(Playlist::Master(url)) = parse_playlist(master, "https://im-fa.manifest.tidal.com/1/m.m3u8") else { panic!() };
        assert_eq!(url, "https://im-fa.manifest.tidal.com/1/manifests/a.m3u8?token=t");
        let media = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MAP:URI=\"https://sp-ad-fa.audio.tidal.com/m/0.mp4?token=a\"\n#EXTINF:3.994,\nhttps://sp-ad-fa.audio.tidal.com/m/1.mp4?token=b\n#EXTINF:3.994,\n2.mp4?token=c\n#EXT-X-ENDLIST\n";
        let Some(Playlist::Media(segments)) = parse_playlist(media, "https://sp-ad-fa.audio.tidal.com/m/index.m3u8") else {
            panic!()
        };
        assert_eq!(segments.len(), 3);
        assert!(segments[0].ends_with("0.mp4?token=a") && segments[2] == "https://sp-ad-fa.audio.tidal.com/m/2.mp4?token=c");
        assert!(parse_playlist("#EXTM3U\nhttps://evil.test/x.mp4\n", "https://sp-ad-fa.audio.tidal.com/m/i.m3u8").is_none());
        assert!(parse_playlist("not a playlist", "https://sp-ad-fa.audio.tidal.com/m/i.m3u8").is_none());
    }
}
