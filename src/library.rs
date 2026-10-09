//! Local library and editable playlists, stored transactionally in SQLite.
//!
//! The database is `library.sqlite3` in the data directory, opened by one
//! process at a time (see `lock.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use lofty::tag::ItemKey;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::i18n::Message;

pub const EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "aiff", "aif", "wma"];
pub const REMOTE_PREFIX: &str = "ytmusic:";

const ARTIST_SEPARATORS: &[&str] =
    &[", ", " & ", "; ", " / ", " feat. ", " feat ", " ft. ", " featuring ", " x ", " vs. ", " with "];

/// The artists of a track, each with the separator that follows it in the original text
/// ("Daft Punk, Romanthony" → [("Daft Punk", ", "), ("Romanthony", "")]).
pub fn artist_parts(value: &str) -> Vec<(String, String)> {
    let lower = value.to_lowercase();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut position = 0;
    while position < value.len() {
        // `lower` keeps the byte positions of `value` for the ASCII separators.
        let found = ARTIST_SEPARATORS
            .iter()
            .find(|sep| lower[position..].starts_with(*sep) && value.is_char_boundary(position + sep.len()));
        match found {
            Some(sep) if position > start => {
                parts.push((value[start..position].trim().to_string(), value[position..position + sep.len()].to_string()));
                position += sep.len();
                start = position;
            }
            _ => position += value[position..].chars().next().map(char::len_utf8).unwrap_or(1),
        }
    }
    let last = value[start..].trim();
    if !last.is_empty() {
        parts.push((last.to_string(), String::new()));
    }
    parts.retain(|(name, _)| !name.is_empty());
    parts
}

/// `true` when `artist` is one of the track's artists (case-insensitive).
pub fn has_artist(value: &str, artist: &str) -> bool {
    let wanted = artist.trim().to_lowercase();
    artist_parts(value).iter().any(|(name, _)| name.to_lowercase() == wanted)
}

/// A YouTube Music item (`ytmusic:<video id>`).
pub fn is_youtube_path(path: &str) -> bool {
    path.starts_with(REMOTE_PREFIX)
}

/// Any library item that is not a local file: YouTube Music, Spotify or TIDAL.
pub fn is_remote_path(path: &str) -> bool {
    is_youtube_path(path) || crate::spotify::is_track_path(path) || crate::tidal::is_track_path(path)
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Track {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// Where each field came from: "tag", "filename" or "missing".
    pub sources: HashMap<&'static str, &'static str>,
}

impl Track {
    pub fn is_remote(&self) -> bool {
        is_remote_path(&self.path)
    }

    pub fn external_path(&self) -> String {
        if is_youtube_path(&self.path) {
            format!("https://music.youtube.com/watch?v={}", &self.path[REMOTE_PREFIX.len()..])
        } else if let Some(url) = crate::spotify::web_url(&self.path) {
            url
        } else if let Some(url) = crate::tidal::web_url(&self.path) {
            url
        } else {
            self.path.clone()
        }
    }

    pub fn field(&self, field: &str) -> &str {
        match field {
            "title" => &self.title,
            "artist" => &self.artist,
            _ => &self.album,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub folder: Option<String>,
    pub count: usize,
}

#[derive(Clone, Debug)]
pub struct FolderMetadataProposal {
    pub folder: String,
    pub targets: BTreeMap<&'static str, Vec<String>>,
    pub values: BTreeMap<&'static str, String>,
}

impl FolderMetadataProposal {
    pub fn targets_for(&self, field: &str) -> &[String] {
        self.targets.get(field).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn value_for(&self, field: &str) -> &str {
        self.values.get(field).map(String::as_str).unwrap_or("")
    }

    pub fn track_count(&self) -> usize {
        let mut all: HashSet<&String> = HashSet::new();
        for paths in self.targets.values() {
            all.extend(paths.iter());
        }
        all.len()
    }
}

fn resolved(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

pub fn read_track(path: &Path) -> Track {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut track = Track { path: resolved(path), title: stem.clone(), ..Default::default() };
    track.sources = HashMap::from([("title", "filename"), ("artist", "missing"), ("album", "missing")]);
    if let Some((artist, title)) = stem.split_once(" - ") {
        if !artist.trim().is_empty() && !title.trim().is_empty() {
            track.title = title.trim().to_string();
            let artist = artist.trim();
            if !artist.chars().all(|c| c.is_ascii_digit()) {
                track.artist = artist.to_string();
                track.sources.insert("artist", "filename");
            }
        }
    }
    if let Ok(file) = lofty::read_from_path(path) {
        track.duration = file.properties().duration().as_secs_f64();
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            for (field, key) in [("title", ItemKey::TrackTitle), ("artist", ItemKey::TrackArtist), ("album", ItemKey::AlbumTitle)]
            {
                if let Some(value) = tag.get_string(key) {
                    let text = value.trim();
                    if !text.is_empty() {
                        match field {
                            "title" => track.title = text.to_string(),
                            "artist" => track.artist = text.to_string(),
                            _ => track.album = text.to_string(),
                        }
                        track.sources.insert(field, "tag");
                    }
                }
            }
        }
    }
    track
}

pub fn has_audio_extension(path: &Path) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .map(|ext| EXTENSIONS.contains(&ext.as_str()))
        .unwrap_or(false)
}

/// Recursively scan a folder. Returns `None` when cancelled.
pub fn scan_folder(
    folder: &Path,
    cancelled: &Arc<AtomicBool>,
    mut progress: impl FnMut(usize),
) -> Result<Option<(Vec<Track>, Vec<String>)>, Message> {
    if !folder.is_dir() {
        return Err(Message::new("folder_unavailable").with("path", folder.to_string_lossy().into_owned()));
    }
    let mut tracks = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let walker = walkdir::WalkDir::new(folder)
        .follow_links(false)
        .sort_by(|a, b| a.file_name().to_string_lossy().to_lowercase().cmp(&b.file_name().to_string_lossy().to_lowercase()));
    for entry in walker {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(error.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() || !has_audio_extension(entry.path()) {
            continue;
        }
        let path = resolved(entry.path());
        if seen.insert(path) {
            tracks.push(read_track(entry.path()));
            if tracks.len() % 50 == 0 {
                progress(tracks.len());
            }
        }
    }
    Ok(Some((tracks, errors)))
}

pub struct Library {
    #[allow(dead_code)]
    pub path: PathBuf,
    db: Connection,
}

const FIELDS: [&str; 3] = ["title", "artist", "album"];

impl Library {
    pub fn open(path: &Path) -> Result<Library> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS tracks (
                path TEXT PRIMARY KEY, title TEXT NOT NULL, artist TEXT NOT NULL,
                album TEXT NOT NULL, duration REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS playlists (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, folder TEXT UNIQUE
            );
            CREATE TABLE IF NOT EXISTS entries (
                playlist INTEGER REFERENCES playlists(id) ON DELETE CASCADE,
                path TEXT REFERENCES tracks(path), position INTEGER NOT NULL,
                PRIMARY KEY (playlist, path)
            );
            CREATE TABLE IF NOT EXISTS discovered (
                playlist INTEGER REFERENCES playlists(id) ON DELETE CASCADE,
                path TEXT NOT NULL, PRIMARY KEY (playlist, path)
            );
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS track_metadata (
                path TEXT REFERENCES tracks(path) ON DELETE CASCADE,
                field TEXT NOT NULL CHECK(field IN ('title', 'artist', 'album')),
                value TEXT NOT NULL, source TEXT NOT NULL,
                confirmed_value TEXT,
                PRIMARY KEY (path, field)
            );",
        )?;
        Ok(Library { path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()), db })
    }

    pub fn playlists(&self) -> Vec<Playlist> {
        let mut statement = match self.db.prepare(
            "SELECT p.id, p.name, p.folder, COUNT(e.path) FROM playlists p
             LEFT JOIN entries e ON p.id=e.playlist GROUP BY p.id ORDER BY p.id",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        statement
            .query_map([], |row| {
                Ok(Playlist { id: row.get(0)?, name: row.get(1)?, folder: row.get(2)?, count: row.get::<_, i64>(3)? as usize })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn playlist(&self, id: i64) -> Option<Playlist> {
        self.playlists().into_iter().find(|p| p.id == id)
    }

    pub fn create_playlist(&mut self, name: &str, folder: Option<&str>, paths: &[String]) -> Result<i64> {
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO playlists(name, folder) VALUES (?1, ?2)", params![name, folder])?;
        let playlist = tx.last_insert_rowid();
        if !paths.is_empty() {
            replace_entries(&tx, playlist, paths)?;
        }
        tx.commit()?;
        Ok(playlist)
    }

    pub fn rename(&mut self, playlist: i64, name: &str) -> Result<()> {
        self.db.execute("UPDATE playlists SET name=?1 WHERE id=?2", params![name, playlist])?;
        Ok(())
    }

    pub fn delete(&mut self, playlist: i64) -> Result<()> {
        self.db.execute("DELETE FROM playlists WHERE id=?1", params![playlist])?;
        Ok(())
    }

    pub fn import_scan(&mut self, folder: &str, tracks: &[Track]) -> Result<i64> {
        let folder = resolved(Path::new(folder));
        let tx = self.db.transaction()?;
        let existing: Option<i64> =
            tx.query_row("SELECT id FROM playlists WHERE folder=?1", params![folder], |row| row.get(0)).optional()?;
        let playlist = match existing {
            Some(id) => id,
            None => {
                let name = Path::new(&folder)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| folder.clone());
                tx.execute("INSERT INTO playlists(name, folder) VALUES (?1, ?2)", params![name, folder])?;
                tx.last_insert_rowid()
            }
        };
        let known: HashSet<String> = {
            let mut statement = tx.prepare("SELECT path FROM discovered WHERE playlist=?1")?;
            let rows: Vec<String> = statement.query_map(params![playlist], |row| row.get(0))?.filter_map(Result::ok).collect();
            rows.into_iter().collect()
        };
        {
            let mut upsert = tx.prepare(
                "INSERT INTO tracks VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(path) DO UPDATE SET
                 title=excluded.title, artist=excluded.artist, album=excluded.album, duration=excluded.duration",
            )?;
            let mut metadata = tx.prepare(
                "INSERT INTO track_metadata(path, field, value, source) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path, field) DO UPDATE SET value=excluded.value, source=excluded.source",
            )?;
            for track in tracks {
                upsert.execute(params![track.path, track.title, track.artist, track.album, track.duration])?;
                for field in FIELDS {
                    let value = track.field(field);
                    let source = track.sources.get(field).copied().unwrap_or(if value.is_empty() { "missing" } else { "tag" });
                    metadata.execute(params![track.path, field, value, source])?;
                }
            }
        }
        apply_confirmed_metadata(&tx, tracks.iter().map(|t| t.path.clone()).collect())?;
        let mut paths: Vec<String> = playlist_paths(&tx, playlist)?;
        let existing: HashSet<String> = paths.iter().cloned().collect();
        for track in tracks {
            if !known.contains(&track.path) && !existing.contains(&track.path) {
                paths.push(track.path.clone());
            }
        }
        replace_entries(&tx, playlist, &paths)?;
        {
            let mut discovered = tx.prepare("INSERT OR IGNORE INTO discovered VALUES (?1, ?2)")?;
            for track in tracks {
                discovered.execute(params![playlist, track.path])?;
            }
        }
        tx.commit()?;
        Ok(playlist)
    }

    pub fn folder_metadata_proposals(&self, paths: &[String], edit: bool) -> Vec<FolderMetadataProposal> {
        #[derive(Clone)]
        struct Record {
            source: String,
            confirmed: Option<String>,
        }
        let mut records: HashMap<(String, String), Record> = HashMap::new();
        let mut selected: Vec<Track> = Vec::new();
        for batch in path_batches(paths) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let args: Vec<&dyn rusqlite::ToSql> = batch.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            if let Ok(mut statement) = self.db.prepare(&format!(
                "SELECT path, field, source, confirmed_value FROM track_metadata WHERE path IN ({placeholders})"
            )) {
                if let Ok(rows) = statement.query_map(args.as_slice(), |row| {
                    Ok((
                        (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        Record { source: row.get(2)?, confirmed: row.get(3)? },
                    ))
                }) {
                    records.extend(rows.filter_map(Result::ok));
                }
            }
            if let Ok(mut statement) = self.db.prepare(&format!("SELECT * FROM tracks WHERE path IN ({placeholders})")) {
                if let Ok(rows) = statement.query_map(args.as_slice(), track_from_row) {
                    selected.extend(rows.filter_map(Result::ok));
                }
            }
        }
        selected.sort_by(|a, b| {
            (a.artist.to_lowercase(), a.title.to_lowercase(), a.path.clone()).cmp(&(
                b.artist.to_lowercase(),
                b.title.to_lowercase(),
                b.path.clone(),
            ))
        });
        let mut groups: BTreeMap<String, Vec<Track>> = BTreeMap::new();
        for track in selected {
            if !track.is_remote() {
                let parent = Path::new(&track.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                groups.entry(parent).or_default().push(track);
            }
        }
        let mut proposals = Vec::new();
        for (folder, tracks) in groups {
            let name = Path::new(&folder).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (mut artist, mut album) = match name.split_once(" - ") {
                Some((artist, album)) if !artist.trim().is_empty() && !album.trim().is_empty() => {
                    (artist.trim().to_string(), album.trim().to_string())
                }
                _ => {
                    if !edit {
                        continue;
                    }
                    (String::new(), String::new())
                }
            };
            let mut targets: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
            let mut values: BTreeMap<&'static str, String> = BTreeMap::new();
            for (field, guess) in [("artist", std::mem::take(&mut artist)), ("album", std::mem::take(&mut album))] {
                let mut paths = Vec::new();
                let mut confirmed: HashSet<String> = HashSet::new();
                for track in &tracks {
                    let record = records.get(&(track.path.clone(), field.to_string()));
                    let value = track.field(field);
                    let source = match record {
                        Some(record) => record.source.as_str(),
                        None if !value.is_empty() => "legacy",
                        None => "missing",
                    };
                    if source == "tag" || source == "legacy" {
                        continue;
                    }
                    if edit || value.is_empty() {
                        paths.push(track.path.clone());
                        if let Some(Record { confirmed: Some(value), .. }) = record {
                            confirmed.insert(value.clone());
                        }
                    }
                }
                let value = if confirmed.len() == 1 { confirmed.into_iter().next().unwrap() } else { guess };
                targets.insert(field, paths);
                values.insert(field, value);
            }
            if !targets["artist"].is_empty() || !targets["album"].is_empty() {
                proposals.push(FolderMetadataProposal { folder, targets, values });
            }
        }
        proposals
    }

    pub fn confirm_folder_metadata(&mut self, proposal: &FolderMetadataProposal, artist: &str, album: &str) -> Result<()> {
        let values = [("artist", artist.trim()), ("album", album.trim())];
        for (field, value) in values {
            if !proposal.targets_for(field).is_empty() && value.is_empty() {
                return Err(anyhow!("Metadata values must not be empty"));
            }
        }
        let tx = self.db.transaction()?;
        let mut paths: Vec<String> = proposal.targets_for("artist").to_vec();
        paths.extend(proposal.targets_for("album").iter().cloned());
        let mut selected: HashMap<String, Track> = HashMap::new();
        for batch in path_batches(&paths) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let args: Vec<&dyn rusqlite::ToSql> = batch.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let mut statement = tx.prepare(&format!("SELECT * FROM tracks WHERE path IN ({placeholders})"))?;
            for track in statement.query_map(args.as_slice(), track_from_row)?.filter_map(Result::ok) {
                selected.insert(track.path.clone(), track);
            }
        }
        for (field, value) in values {
            for path in proposal.targets_for(field) {
                let track = selected.get(path);
                let parent = Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let Some(track) = track else { return Err(anyhow!("Track does not belong to the proposed folder")) };
                if track.is_remote() || parent != proposal.folder {
                    return Err(anyhow!("Track does not belong to the proposed folder"));
                }
                let raw = track.field(field);
                tx.execute(
                    "INSERT OR IGNORE INTO track_metadata(path, field, value, source) VALUES (?1, ?2, ?3, ?4)",
                    params![path, field, raw, if raw.is_empty() { "missing" } else { "legacy" }],
                )?;
                tx.execute(
                    "UPDATE track_metadata SET confirmed_value=?1 WHERE path=?2 AND field=?3 AND source NOT IN ('tag', 'legacy')",
                    params![value, path, field],
                )?;
            }
        }
        apply_confirmed_metadata(&tx, paths)?;
        tx.commit()?;
        Ok(())
    }

    fn save_remote_tracks(db: &Connection, tracks: &[Track]) -> Result<()> {
        let mut upsert = db.prepare(
            "INSERT INTO tracks VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(path) DO UPDATE SET
             title=excluded.title, artist=excluded.artist, album=excluded.album, duration=excluded.duration",
        )?;
        for track in tracks {
            let id = track.path.strip_prefix(REMOTE_PREFIX).unwrap_or("");
            let youtube = is_youtube_path(&track.path)
                && id.len() == 11
                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !youtube && !crate::spotify::is_track_path(&track.path) && !crate::tidal::is_track_path(&track.path) {
                return Err(anyhow!("Only YouTube Music, Spotify and TIDAL tracks can be imported here"));
            }
            upsert.execute(params![track.path, track.title, track.artist, track.album, track.duration])?;
        }
        Ok(())
    }

    /// One transaction: never leave a partial import after an error.
    pub fn import_remote_playlist(&mut self, name: &str, tracks: &[Track]) -> Result<i64> {
        let tx = self.db.transaction()?;
        Self::save_remote_tracks(&tx, tracks)?;
        tx.execute("INSERT INTO playlists(name) VALUES (?1)", params![name])?;
        let playlist = tx.last_insert_rowid();
        let paths: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
        replace_entries(&tx, playlist, &paths)?;
        tx.commit()?;
        Ok(playlist)
    }

    pub fn add_remote_tracks(&mut self, tracks: &[Track], playlist: Option<i64>) -> Result<()> {
        let tx = self.db.transaction()?;
        Self::save_remote_tracks(&tx, tracks)?;
        if let Some(playlist) = playlist {
            let added: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
            let paths = with_added_on_top(&playlist_paths(&tx, playlist)?, &added);
            replace_entries(&tx, playlist, &paths)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn tracks(&self, playlist: Option<i64>) -> Vec<Track> {
        let result = match playlist {
            None => self
                .db
                .prepare("SELECT * FROM tracks ORDER BY artist COLLATE NOCASE, title COLLATE NOCASE")
                .and_then(|mut s| s.query_map([], track_from_row).map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())),
            Some(id) => self
                .db
                .prepare("SELECT t.* FROM tracks t JOIN entries e ON t.path=e.path WHERE e.playlist=?1 ORDER BY e.position")
                .and_then(|mut s| {
                    s.query_map(params![id], track_from_row).map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
                }),
        };
        result.unwrap_or_default()
    }

    pub fn first_path(&self, playlist: i64) -> Option<String> {
        self.db
            .query_row("SELECT path FROM entries WHERE playlist=?1 ORDER BY position LIMIT 1", params![playlist], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    pub fn track(&self, path: &str) -> Option<Track> {
        self.db.query_row("SELECT * FROM tracks WHERE path=?1", params![path], track_from_row).optional().ok().flatten()
    }

    pub fn reorder(&mut self, playlist: i64, paths: &[String]) -> Result<()> {
        let tx = self.db.transaction()?;
        replace_entries(&tx, playlist, paths)?;
        tx.commit()?;
        Ok(())
    }

    /// Add tracks at the top of a playlist (newest first), see [`with_added_on_top`].
    pub fn add(&mut self, playlist: i64, paths: &[String]) -> Result<()> {
        let existing: Vec<String> = self.tracks(Some(playlist)).into_iter().map(|t| t.path).collect();
        self.reorder(playlist, &with_added_on_top(&existing, paths))
    }

    pub fn remove(&mut self, playlist: i64, paths: &[String]) -> Result<()> {
        let remove: HashSet<&String> = paths.iter().collect();
        let kept: Vec<String> = self.tracks(Some(playlist)).into_iter().map(|t| t.path).filter(|p| !remove.contains(p)).collect();
        self.reorder(playlist, &kept)
    }

    pub fn setting(&self, key: &str) -> Option<Value> {
        let text: Option<String> = self
            .db
            .query_row("SELECT value FROM settings WHERE key=?1", params![key], |row| row.get(0))
            .optional()
            .ok()
            .flatten();
        text.and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn setting_bool(&self, key: &str, default: bool) -> bool {
        self.setting(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }

    pub fn setting_f64(&self, key: &str, default: f64) -> f64 {
        self.setting(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }

    pub fn setting_string(&self, key: &str) -> Option<String> {
        self.setting(key).and_then(|v| v.as_str().map(str::to_string))
    }

    pub fn setting_strings(&self, key: &str) -> Vec<String> {
        self.setting(key)
            .and_then(|v| v.as_array().cloned())
            .map(|items| items.into_iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }

    pub fn setting_ids(&self, key: &str) -> Vec<i64> {
        self.setting(key)
            .and_then(|v| v.as_array().cloned())
            .map(|items| items.into_iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default()
    }

    pub fn set_setting(&mut self, key: &str, value: Value) {
        let _ = self.db.execute("INSERT OR REPLACE INTO settings VALUES (?1, ?2)", params![key, value.to_string()]);
    }
}

fn track_from_row(row: &rusqlite::Row) -> rusqlite::Result<Track> {
    Ok(Track {
        path: row.get("path")?,
        title: row.get("title")?,
        artist: row.get("artist")?,
        album: row.get("album")?,
        duration: row.get("duration")?,
        sources: HashMap::new(),
    })
}

fn playlist_paths(db: &Connection, playlist: i64) -> Result<Vec<String>> {
    let mut statement = db.prepare("SELECT path FROM entries WHERE playlist=?1 ORDER BY position")?;
    let paths = statement.query_map(params![playlist], |row| row.get(0))?.filter_map(Result::ok).collect();
    Ok(paths)
}

fn replace_entries(db: &Connection, playlist: i64, paths: &[String]) -> Result<()> {
    db.execute("DELETE FROM entries WHERE playlist=?1", params![playlist])?;
    let mut insert = db.prepare("INSERT INTO entries VALUES (?1, ?2, ?3)")?;
    let mut seen = HashSet::new();
    let mut position = 0;
    for path in paths {
        if seen.insert(path) {
            insert.execute(params![playlist, path, position])?;
            position += 1;
        }
    }
    Ok(())
}

/// Stay below even SQLite's older 999-variable limit for large selections.
fn path_batches(paths: &[String]) -> Vec<Vec<String>> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        if seen.insert(path) {
            unique.push(path.clone());
        }
    }
    unique.chunks(500).map(|c| c.to_vec()).collect()
}

fn apply_confirmed_metadata(db: &Connection, paths: Vec<String>) -> Result<()> {
    let assignments = ["artist", "album"]
        .iter()
        .map(|field| {
            format!(
                "{field}=COALESCE((SELECT confirmed_value FROM track_metadata m WHERE m.path=tracks.path AND m.field='{field}' AND m.source NOT IN ('tag', 'legacy')), {field})"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    for batch in path_batches(&paths) {
        let placeholders = vec!["?"; batch.len()].join(",");
        let args: Vec<&dyn rusqlite::ToSql> = batch.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        db.execute(&format!("UPDATE tracks SET {assignments} WHERE path IN ({placeholders})"), args.as_slice())?;
    }
    Ok(())
}

pub fn format_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub fn format_clock(ms: u64) -> String {
    let t = ms / 1000;
    format!("{}:{:02}", t / 60, t % 60)
}

/// The order of a playlist after adding `added`: the added tracks first, in the given
/// order, then the rest as before. A track that was already in the playlist moves up.
fn with_added_on_top(existing: &[String], added: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut order: Vec<String> = added.iter().filter(|p| seen.insert(p.as_str())).cloned().collect();
    order.extend(existing.iter().filter(|p| !seen.contains(p.as_str())).cloned());
    order
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn library() -> Library {
        let dir = std::env::temp_dir().join(format!("fono8-test-{}-{}", std::process::id(), rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        Library::open(&dir.join("library.sqlite3")).unwrap()
    }

    fn track(path: &str, artist: &str, album: &str) -> Track {
        Track {
            path: path.into(),
            title: "T".into(),
            artist: artist.into(),
            album: album.into(),
            duration: 1.0,
            sources: HashMap::from([
                ("title", "filename"),
                ("artist", if artist.is_empty() { "missing" } else { "tag" }),
                ("album", if album.is_empty() { "missing" } else { "tag" }),
            ]),
        }
    }

    #[test]
    fn import_keeps_order_and_skips_removed() {
        let mut lib = library();
        let folder = std::env::temp_dir().join("fono8-music");
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let a = format!("{folder}/a.mp3");
        let b = format!("{folder}/b.mp3");
        let id = lib.import_scan(&folder, &[track(&a, "", ""), track(&b, "", "")]).unwrap();
        assert_eq!(lib.tracks(Some(id)).len(), 2);
        lib.remove(id, &[a.clone()]).unwrap();
        lib.import_scan(&folder, &[track(&a, "", ""), track(&b, "", "")]).unwrap();
        let paths: Vec<_> = lib.tracks(Some(id)).into_iter().map(|t| t.path).collect();
        assert_eq!(paths, vec![b.clone()]);
        assert_eq!(lib.playlists()[0].count, 1);
    }

    #[test]
    fn folder_metadata_proposal_and_confirmation() {
        let mut lib = library();
        let folder = std::env::temp_dir().join("fono8-meta").join("Artist - Album");
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let a = format!("{folder}/01.mp3");
        lib.import_scan(&folder, &[track(&a, "", "")]).unwrap();
        let proposals = lib.folder_metadata_proposals(&[a.clone()], false);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].value_for("artist"), "Artist");
        assert_eq!(proposals[0].value_for("album"), "Album");
        lib.confirm_folder_metadata(&proposals[0], "Artist", "Album").unwrap();
        let saved = lib.track(&a).unwrap();
        assert_eq!(saved.artist, "Artist");
        assert_eq!(saved.album, "Album");
        // Rescanning keeps the confirmed values.
        lib.import_scan(&folder, &[track(&a, "", "")]).unwrap();
        assert_eq!(lib.track(&a).unwrap().artist, "Artist");
        assert!(lib.folder_metadata_proposals(&[a.clone()], false).is_empty());
    }

    /// A minimal silent 16-bit mono WAV file.
    pub(crate) fn write_wav(path: &std::path::Path, seconds: u32) {
        let rate: u32 = 8000;
        let samples = rate * seconds;
        let data_len = samples * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + data_len as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scan_reads_files_and_proposes_folder_metadata() {
        let mut lib = library();
        let root = std::env::temp_dir().join(format!("fono8-scan-{}-{}", std::process::id(), rand::random::<u32>()));
        let album = root.join("Scan Artist - Scan Album");
        std::fs::create_dir_all(&album).unwrap();
        write_wav(&album.join("01 - First.wav"), 2);
        write_wav(&album.join("Named Artist - Second.wav"), 1);
        std::fs::write(album.join("notes.txt"), "ignored").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tracks, errors) = scan_folder(&root, &cancelled, |_| {}).unwrap().unwrap();
        assert!(errors.is_empty());
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "First");
        assert_eq!(tracks[0].artist, "");
        assert!((tracks[0].duration - 2.0).abs() < 0.01);
        assert_eq!(tracks[1].artist, "Named Artist");
        let playlist = lib.import_scan(&root.to_string_lossy(), &tracks).unwrap();
        assert_eq!(lib.playlists()[0].name, root.file_name().unwrap().to_string_lossy());
        assert_eq!(lib.tracks(Some(playlist)).len(), 2);
        let paths: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
        let proposals = lib.folder_metadata_proposals(&paths, false);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].value_for("artist"), "Scan Artist");
        assert_eq!(proposals[0].targets_for("artist").len(), 1, "filename artist is kept");
        assert_eq!(proposals[0].targets_for("album").len(), 2);
        // Editing proposes every non-tag field, even ones already filled from file names.
        assert_eq!(lib.folder_metadata_proposals(&paths, true)[0].targets_for("artist").len(), 2);
    }

    #[test]
    fn mixed_playlists_and_atomic_failed_import() {
        let mut lib = library();
        let remote = |id: &str| Track {
            path: format!("ytmusic:{id}"),
            title: "Remote".into(),
            artist: "A".into(),
            album: String::new(),
            duration: 10.0,
            sources: HashMap::new(),
        };
        let id = lib.import_remote_playlist("Mix", &[remote("abcdefghijk"), remote("lmnopqrstuv")]).unwrap();
        assert_eq!(lib.tracks(Some(id)).len(), 2);
        assert!(lib.tracks(Some(id))[0].is_remote());
        // An invalid id aborts the whole import without creating a playlist.
        assert!(lib.import_remote_playlist("Bad", &[remote("abcdefghijk"), remote("short")]).is_err());
        assert_eq!(lib.playlists().len(), 1);
        let local = track("/tmp/fono8-local.mp3", "L", "");
        lib.import_scan("/tmp", &[local.clone()]).unwrap();
        lib.add(id, &[local.path.clone()]).unwrap();
        lib.add_remote_tracks(&[remote("zzzzzzzzzzz")], Some(id)).unwrap();
        let paths: Vec<String> = lib.tracks(Some(id)).into_iter().map(|t| t.path).collect();
        // Added tracks go to the top, newest first.
        assert_eq!(paths, vec!["ytmusic:zzzzzzzzzzz", local.path.as_str(), "ytmusic:abcdefghijk", "ytmusic:lmnopqrstuv"]);
    }

    #[test]
    fn added_tracks_go_to_the_top_in_their_order() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(with_added_on_top(&list(&["a", "b"]), &list(&["x", "y"])), list(&["x", "y", "a", "b"]));
        assert_eq!(
            with_added_on_top(&list(&["a", "b", "c"]), &list(&["c"])),
            list(&["c", "a", "b"]),
            "an existing track moves up"
        );
        assert_eq!(with_added_on_top(&list(&["a"]), &list(&["x", "x"])), list(&["x", "a"]), "no duplicates");
        assert_eq!(with_added_on_top(&[], &list(&["x"])), list(&["x"]));
    }

    #[test]
    fn settings_roundtrip() {
        let mut lib = library();
        lib.set_setting("volume", serde_json::json!(0.5));
        assert_eq!(lib.setting_f64("volume", 1.0), 0.5);
        lib.set_setting("pinned_playlists", serde_json::json!([3, 4]));
        assert_eq!(lib.setting_ids("pinned_playlists"), vec![3, 4]);
    }

    #[test]
    fn splits_artists_and_keeps_separators() {
        assert_eq!(
            artist_parts("Daft Punk, Romanthony"),
            [("Daft Punk".to_string(), ", ".to_string()), ("Romanthony".to_string(), String::new())]
        );
        let names: Vec<String> = artist_parts("JIMEK feat. Łona & Webber").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["JIMEK", "Łona", "Webber"]);
        assert_eq!(
            artist_parts("Simon & Garfunkel feat"),
            [("Simon".to_string(), " & ".to_string()), ("Garfunkel feat".to_string(), String::new())]
        );
        assert_eq!(artist_parts("Iron Jarl").len(), 1);
        assert!(artist_parts("").is_empty());
        assert!(has_artist("Daft Punk, Romanthony", "daft punk"));
        assert!(!has_artist("Daft Punk, Romanthony", "Daft"));
    }
}
