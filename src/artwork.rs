//! Asynchronous, bounded local artwork loading.
//!
//! Covers come from `cover.jpg`/`cover.png`/`folder.jpg`/`folder.png`/`front.jpg`
//! next to the file, or from pictures embedded in the tags. Images are decoded
//! on a worker thread, scaled to at most 320 px and cached as GPU-ready frames.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use gpui::RenderImage;
use image::{imageops::FilterType, Frame};
use lofty::file::TaggedFileExt;
use smallvec::SmallVec;

use crate::library::is_remote_path;

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PIXELS: u64 = 40_000_000;
const COVER_FILES: &[&str] = &["cover.jpg", "cover.png", "folder.jpg", "folder.png", "front.jpg"];

pub struct Artwork {
    loaded: HashMap<String, Option<Arc<RenderImage>>>,
    requested: HashSet<String>,
    jobs: Sender<Option<String>>,
    results: Arc<Mutex<Vec<(String, Option<Arc<RenderImage>>)>>>,
}

impl Artwork {
    pub fn new() -> Artwork {
        let (jobs, receiver) = mpsc::channel::<Option<String>>();
        let results = Arc::new(Mutex::new(Vec::new()));
        let shared = results.clone();
        std::thread::Builder::new().name("fono8-artwork".into()).spawn(move || worker(receiver, shared)).expect("artwork thread");
        Artwork { loaded: HashMap::new(), requested: HashSet::new(), jobs, results }
    }

    /// The cover for a track path, requesting it in the background on first use.
    /// YouTube Music tracks get their `i.ytimg.com` thumbnail.
    pub fn get(&mut self, path: &str) -> Option<Arc<RenderImage>> {
        if path.is_empty() {
            return None;
        }
        if let Some(image) = self.loaded.get(path) {
            return image.clone();
        }
        if self.requested.insert(path.to_string()) {
            let _ = self.jobs.send(Some(path.to_string()));
        }
        None
    }

    /// Collect finished jobs; returns `true` when anything new arrived.
    pub fn poll(&mut self) -> bool {
        let finished: Vec<_> = match self.results.lock() {
            Ok(mut results) => std::mem::take(&mut *results),
            Err(_) => return false,
        };
        let changed = !finished.is_empty();
        for (path, image) in finished {
            self.loaded.insert(path, image);
        }
        changed
    }
}

impl Drop for Artwork {
    fn drop(&mut self) {
        let _ = self.jobs.send(None);
    }
}

fn worker(receiver: Receiver<Option<String>>, results: Arc<Mutex<Vec<(String, Option<Arc<RenderImage>>)>>>) {
    while let Ok(Some(path)) = receiver.recv() {
        let image = read(&path).map(Arc::new);
        if let Ok(mut results) = results.lock() {
            results.push((path, image));
        }
    }
}

fn read(path: &str) -> Option<RenderImage> {
    if is_remote_path(path) {
        return read_thumbnail(path);
    }
    let parent = Path::new(path).parent()?;
    for name in COVER_FILES {
        let candidate = parent.join(name);
        if let Ok(metadata) = candidate.metadata() {
            if metadata.is_file() && metadata.len() <= MAX_BYTES {
                if let Some(image) = std::fs::read(&candidate).ok().and_then(|bytes| decode(&bytes)) {
                    return Some(image);
                }
            }
        }
    }
    let file = lofty::read_from_path(path).ok()?;
    for tag in file.tags() {
        if let Some(picture) = tag.pictures().first() {
            let data = picture.data();
            if !data.is_empty() && data.len() as u64 <= MAX_BYTES {
                if let Some(image) = decode(data) {
                    return Some(image);
                }
            }
        }
    }
    None
}

fn read_thumbnail(path: &str) -> Option<RenderImage> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .max_redirects(0)
        .build()
        .new_agent();
    let url = match crate::spotify::web_url(path) {
        Some(page) => spotify_thumbnail(&agent, &page)?,
        None => format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", crate::youtube::models::video_id(path).ok()?),
    };
    let mut response = agent.get(&url).call().ok()?;
    let bytes = response.body_mut().with_config().limit(MAX_BYTES).read_to_vec().ok()?;
    decode(&bytes)
}

/// The album cover of a Spotify track from the public oEmbed endpoint (no sign-in needed).
fn spotify_thumbnail(agent: &ureq::Agent, page: &str) -> Option<String> {
    let query: String = form_urlencoded::Serializer::new(String::new()).append_pair("url", page).finish();
    let mut response = agent.get(&format!("https://open.spotify.com/oembed?{query}")).call().ok()?;
    let bytes = response.body_mut().with_config().limit(64 * 1024).read_to_vec().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let thumbnail = url::Url::parse(value.get("thumbnail_url")?.as_str()?).ok()?;
    let host = thumbnail.host_str()?;
    let trusted = host.ends_with(".spotifycdn.com") || host.ends_with(".scdn.co");
    (thumbnail.scheme() == "https" && trusted && thumbnail.port().is_none()).then(|| thumbnail.to_string())
}

fn decode(bytes: &[u8]) -> Option<RenderImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 || (width as u64) * (height as u64) > MAX_PIXELS {
        return None;
    }
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > 320 || image.height() > 320 { image.resize(320, 320, FilterType::Triangle) } else { image };
    let mut rgba = image.into_rgba8();
    // GPUI expects premultiplied BGRA.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        let alpha = pixel[3] as u32;
        if alpha < 255 {
            pixel[0] = ((pixel[0] as u32 * alpha) / 255) as u8;
            pixel[1] = ((pixel[1] as u32 * alpha) / 255) as u8;
            pixel[2] = ((pixel[2] as u32 * alpha) / 255) as u8;
        }
    }
    Some(RenderImage::new(SmallVec::from_elem(Frame::new(rgba), 1)))
}
