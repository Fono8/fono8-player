//! Importing lists from a service Fono8 cannot fully play (TIDAL): every track
//! is matched to a playable one on YouTube Music (by artist and title) and on
//! Spotify (by ISRC), in the order the user chose. This module holds the
//! matching rules and the progress of one import; `app.rs` runs the searches.

use crate::library::{artist_parts, Track};
use crate::playback::Remote;

/// Normalized words of a title or artist: lowercase, letters and digits only,
/// without bracketed parts such as "(feat. …)", "(Official Video)" or "[Remastered]".
fn words(value: &str) -> Vec<String> {
    let mut plain = String::new();
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_alphanumeric() => plain.extend(c.to_lowercase()),
            _ => plain.push(' '),
        }
    }
    const NOISE: &[&str] =
        &["feat", "ft", "featuring", "official", "video", "audio", "lyrics", "remastered", "remaster", "hd", "mv"];
    plain.split_whitespace().filter(|w| !NOISE.contains(w)).map(str::to_string).collect()
}

/// Share of the wanted title's words found in the candidate (0..=1).
fn title_score(wanted: &str, candidate: &str) -> f64 {
    let wanted = words(wanted);
    if wanted.is_empty() {
        return 0.0;
    }
    let candidate = words(candidate);
    wanted.iter().filter(|w| candidate.contains(w)).count() as f64 / wanted.len() as f64
}

/// One of the wanted artists appears in the candidate's artist or title.
fn artist_matches(wanted: &str, candidate: &Track) -> bool {
    let haystack = words(&format!("{} {}", candidate.artist, candidate.title)).join(" ");
    artist_parts(wanted).iter().any(|(name, _)| {
        let name = words(name).join(" ");
        !name.is_empty() && format!(" {haystack} ").contains(&format!(" {name} "))
    })
}

/// Durations agree when both are known and differ by at most 10 s (or one is unknown).
fn durations_agree(wanted: f64, candidate: f64) -> bool {
    wanted <= 0.0 || candidate <= 0.0 || (wanted - candidate).abs() <= 10.0
}

/// The best YouTube Music result for `wanted`, if one is close enough.
pub fn pick_by_name(wanted: &Track, candidates: &[Track]) -> Option<Track> {
    candidates
        .iter()
        .filter(|c| artist_matches(&wanted.artist, c) && durations_agree(wanted.duration, c.duration))
        .map(|c| (title_score(&wanted.title, &c.title), c))
        .filter(|(score, _)| *score >= 0.75)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c.clone())
}

/// A Spotify result for an ISRC search: the recording is the same, so only the duration is checked.
pub fn pick_by_isrc(wanted: &Track, candidates: &[Track]) -> Option<Track> {
    candidates.iter().find(|c| durations_agree(wanted.duration, c.duration)).cloned()
}

/// One TIDAL track to match.
#[derive(Clone, Debug, PartialEq)]
pub struct Wanted {
    pub track: Track,
    pub isrc: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The next search has not been sent yet (e.g. the engine was busy).
    Pending,
    YouTube,
    Spotify,
}

/// The progress of one import.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub name: String,
    pub enqueue: bool,
    pub wanted: Vec<Wanted>,
    pub index: usize,
    pub stage: Stage,
    pub matched: Vec<Track>,
    pub missing: Vec<String>,
    pub from_youtube: usize,
    pub from_spotify: usize,
    /// The engines to try for each track, in order.
    pub order: Vec<Remote>,
    /// How many engines were tried for the current track.
    pub attempt: usize,
}

impl Transfer {
    pub fn new(name: String, wanted: Vec<Wanted>, enqueue: bool, order: Vec<Remote>) -> Transfer {
        Transfer {
            name,
            enqueue,
            wanted,
            index: 0,
            stage: Stage::Pending,
            matched: Vec::new(),
            missing: Vec::new(),
            from_youtube: 0,
            from_spotify: 0,
            order,
            attempt: 0,
        }
    }

    /// The next engine to try for the current track, if any is left.
    pub fn next_engine(&mut self) -> Option<Remote> {
        let engine = self.order.get(self.attempt).copied();
        self.attempt += 1;
        engine
    }

    /// Try the same engine again later (it was busy or still loading).
    pub fn retry_engine(&mut self) {
        self.attempt = self.attempt.saturating_sub(1);
        self.stage = Stage::Pending;
    }

    pub fn current(&self) -> Option<&Wanted> {
        self.wanted.get(self.index)
    }

    pub fn done(&self) -> bool {
        self.index >= self.wanted.len()
    }

    /// Record the result for the current track and move on.
    pub fn resolve(&mut self, found: Option<(Track, Remote)>) {
        let Some(wanted) = self.current().cloned() else { return };
        match found {
            Some((mut track, remote)) => {
                if track.album.is_empty() {
                    track.album = wanted.track.album.clone();
                }
                if !self.matched.iter().any(|t| t.path == track.path) {
                    self.matched.push(track);
                }
                match remote {
                    Remote::YouTube => self.from_youtube += 1,
                    Remote::Spotify => self.from_spotify += 1,
                }
            }
            None => self.missing.push(format!("{} – {}", wanted.track.artist, wanted.track.title)),
        }
        self.index += 1;
        self.attempt = 0;
        self.stage = Stage::Pending;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, title: &str, artist: &str, duration: f64) -> Track {
        Track { path: path.into(), title: title.into(), artist: artist.into(), duration, ..Default::default() }
    }

    #[test]
    fn picks_the_matching_youtube_result_and_ignores_covers_and_videos() {
        let wanted = track("tidal:track:1", "The Day That Never Comes", "Metallica", 476.0);
        let candidates = [
            track("ytmusic:aaaaaaaaaaa", "The Day That Never Comes (Cover)", "Some Band", 470.0),
            track("ytmusic:bbbbbbbbbbb", "The Day That Never Comes (Official Music Video)", "Metallica", 520.0),
            track("ytmusic:ccccccccccc", "The Day That Never Comes", "Metallica", 476.0),
        ];
        assert_eq!(pick_by_name(&wanted, &candidates).unwrap().path, "ytmusic:ccccccccccc");
        let featured = track("tidal:track:2", "Etiuda nr7 (feat. Łona)", "JIMEK, Łona", 200.0);
        let found = [track("ytmusic:ddddddddddd", "Etiuda nr7", "JIMEK", 201.0)];
        assert!(pick_by_name(&featured, &found).is_some());
        let unrelated = [track("ytmusic:eeeeeeeeeee", "Something else", "Metallica", 476.0)];
        assert!(pick_by_name(&wanted, &unrelated).is_none());
    }

    #[test]
    fn isrc_results_only_need_a_matching_duration() {
        let wanted = track("tidal:track:1", "One More Time", "Daft Punk", 320.0);
        let candidates = [
            track("spotify:track:x", "One More Time - Radio Edit", "Daft Punk", 230.0),
            track("spotify:track:y", "One More Time", "Daft Punk", 321.0),
        ];
        assert_eq!(pick_by_isrc(&wanted, &candidates).unwrap().path, "spotify:track:y");
    }

    #[test]
    fn transfer_records_matches_and_misses_in_order() {
        let wanted = |n: &str| Wanted { track: track(&format!("tidal:track:{n}"), n, "A", 0.0), isrc: None };
        let mut transfer = Transfer::new(
            "Mix".into(),
            vec![wanted("1"), wanted("2"), wanted("3")],
            false,
            vec![Remote::Spotify, Remote::YouTube],
        );
        assert_eq!(transfer.next_engine(), Some(Remote::Spotify));
        transfer.retry_engine();
        assert_eq!(transfer.next_engine(), Some(Remote::Spotify));
        assert_eq!(transfer.next_engine(), Some(Remote::YouTube));
        assert_eq!(transfer.next_engine(), None);
        transfer.resolve(Some((track("ytmusic:aaaaaaaaaaa", "1", "A", 0.0), Remote::YouTube)));
        transfer.resolve(None);
        transfer.resolve(Some((track("spotify:track:z", "3", "A", 0.0), Remote::Spotify)));
        assert!(transfer.done());
        assert_eq!(transfer.matched.len(), 2);
        assert_eq!(transfer.missing, ["A – 2"]);
        assert_eq!((transfer.from_youtube, transfer.from_spotify), (1, 1));
    }
}
