//! Playback queue: ordering, shuffle, repeat and history, on top of `AudioEngine`.
//!
//! The queue is a list of track paths
//! without duplicates, `index` is the current item, `history` records visited
//! indices for shuffle/previous, and `failed` holds items that could not play.

use std::collections::HashSet;
use std::path::Path;

use rand::seq::IndexedRandom;

use crate::audio::{AudioEngine, AudioSnapshot, PlaybackState};
use crate::i18n::Message;
use crate::library::is_youtube_path;
use crate::spotify::player::{PlayerEvent, SpotifyLink};
use crate::youtube::{models::video_id, HelperEvent, HelperLink};

/// Which engine plays a queue item that is not a local file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    YouTube,
    Spotify,
}

pub fn remote_kind(path: &str) -> Option<Remote> {
    if is_youtube_path(path) {
        Some(Remote::YouTube)
    } else if crate::spotify::is_track_path(path) {
        Some(Remote::Spotify)
    } else {
        None
    }
}

/// The controls shared by the YouTube Music helper and the Spotify player.
pub trait RemoteControl {
    fn load(&self, id: &str) -> bool;
    fn play(&self);
    fn pause(&self);
    fn stop(&self);
    fn seek(&self, ms: u64);
    fn set_volume(&self, volume: f32);
    fn set_muted(&self, muted: bool);
}

macro_rules! remote_control {
    ($type:ty) => {
        impl RemoteControl for $type {
            fn load(&self, id: &str) -> bool {
                <$type>::load(self, id)
            }
            fn play(&self) {
                <$type>::play(self)
            }
            fn pause(&self) {
                <$type>::pause(self)
            }
            fn stop(&self) {
                <$type>::stop(self)
            }
            fn seek(&self, ms: u64) {
                <$type>::seek(self, ms)
            }
            fn set_volume(&self, volume: f32) {
                <$type>::set_volume(self, volume)
            }
            fn set_muted(&self, muted: bool) {
                <$type>::set_muted(self, muted)
            }
        }
    };
}

remote_control!(HelperLink);
remote_control!(SpotifyLink);

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    /// The TIDAL preview of this track must be downloaded first (then call `retry_current`).
    NeedsPreview(String),
    /// The YouTube Music engine is not running yet: start it, then call `retry_current`.
    /// Happens when the queue reaches a YouTube Music item on its own, e.g. after a Spotify track.
    NeedsYouTube(String),
    TrackChanged(String),
    QueueChanged,
    StateChanged(PlaybackState),
    Error(Message),
}

pub struct Playback {
    pub engine: AudioEngine,
    pub queue: Vec<String>,
    pub index: Option<usize>,
    pub shuffle: bool,
    pub repeat: bool,
    pub muted: bool,
    pub volume: f64,
    history: Vec<usize>,
    failed: HashSet<String>,
    pub events: Vec<PlaybackEvent>,
    /// Snapshot captured on the last tick.
    pub snapshot: AudioSnapshot,
    last_state: PlaybackState,
    /// The audio generation of the currently loaded track, used to ignore stale ends.
    loaded_generation: u64,
    /// Duration from the library, used when the decoder reports none.
    duration_hint: f64,
    /// The YouTube Music helper, once started; `ytmusic:` tracks are driven through it.
    pub remote: Option<HelperLink>,
    /// The Spotify player, once signed in; `spotify:track:` items are driven through it.
    pub spotify: Option<SpotifyLink>,
    /// The engine playing the current queue item, when it is not the local one.
    remote_active: Option<Remote>,
    /// Fono8 Cast is sending local audio to a speaker; Spotify cannot join it.
    pub casting: bool,
    /// Where TIDAL previews are cached (`<id>.mp4`); they play like local files.
    pub preview_dir: Option<std::path::PathBuf>,
}

impl Playback {
    pub fn new() -> Playback {
        Playback {
            engine: AudioEngine::start(),
            queue: Vec::new(),
            index: None,
            shuffle: false,
            repeat: false,
            muted: false,
            volume: 0.65,
            history: Vec::new(),
            failed: HashSet::new(),
            events: Vec::new(),
            snapshot: AudioSnapshot::default(),
            last_state: PlaybackState::Stopped,
            loaded_generation: 0,
            duration_hint: 0.0,
            remote: None,
            spotify: None,
            remote_active: None,
            casting: false,
            preview_dir: None,
        }
    }

    pub fn current(&self) -> Option<&str> {
        self.index.and_then(|i| self.queue.get(i)).map(String::as_str)
    }

    /// The engine of the current item, if it is not a local file.
    pub fn current_remote(&self) -> Option<Remote> {
        self.current().and_then(remote_kind)
    }

    fn link(&self, kind: Remote) -> Option<&dyn RemoteControl> {
        match kind {
            Remote::YouTube => self.remote.as_ref().map(|link| link as &dyn RemoteControl),
            Remote::Spotify => self.spotify.as_ref().map(|link| link as &dyn RemoteControl),
        }
    }

    fn active_link(&self) -> Option<&dyn RemoteControl> {
        self.remote_active.and_then(|kind| self.link(kind))
    }

    fn remote_progress(&mut self, state: PlaybackState, position: u64, duration: u64) {
        self.snapshot.position_ms = position;
        if duration > 0 {
            self.snapshot.duration_ms = duration;
        }
        self.set_state(state);
    }

    fn remote_ended(&mut self) {
        self.remote_active = None;
        self.set_state(PlaybackState::Stopped);
        self.next(true);
    }

    /// Apply an event from the Spotify player to the track being played.
    pub fn spotify_event(&mut self, event: &PlayerEvent) {
        if self.remote_active != Some(Remote::Spotify) {
            return;
        }
        match event {
            PlayerEvent::State { playing, position_ms, duration_ms } => {
                let state = if *playing { PlaybackState::Playing } else { PlaybackState::Paused };
                self.remote_progress(state, *position_ms, *duration_ms);
            }
            PlayerEvent::Ended => self.remote_ended(),
            PlayerEvent::Error(message) => {
                self.remote_active = None;
                self.events.push(PlaybackEvent::Error(message.clone()));
                // Account, sign-in and DRM problems are not broken songs: keep the item for a retry.
                let track_problem =
                    matches!(message.key, "spotify_playback_error" | "spotify_not_found" | "spotify_start_timeout");
                if track_problem {
                    self.error_after_remote();
                } else {
                    self.set_state(PlaybackState::Stopped);
                }
            }
            PlayerEvent::Exited => {
                self.remote_active = None;
                self.events.push(PlaybackEvent::Error(Message::new("spotify_sdk_failed")));
                self.set_state(PlaybackState::Stopped);
            }
        }
    }

    /// Apply an event from the helper to the remote track being played.
    pub fn remote_event(&mut self, event: &HelperEvent) {
        if self.remote_active != Some(Remote::YouTube) {
            return;
        }
        match event {
            HelperEvent::Player { state, position, duration } => {
                let state = match state.as_str() {
                    "playing" => PlaybackState::Playing,
                    "paused" => PlaybackState::Paused,
                    _ => PlaybackState::Stopped,
                };
                self.remote_progress(state, *position, *duration);
            }
            HelperEvent::Ended => self.remote_ended(),
            HelperEvent::Error { key } => {
                let key: &'static str = match key.as_str() {
                    "yt_login_required" => "yt_login_required",
                    "yt_auth_check_failed" => "yt_auth_check_failed",
                    "yt_playback_interrupted" => "yt_playback_interrupted",
                    "yt_playback_error" => "yt_playback_error",
                    "yt_browser_error" => "yt_browser_error",
                    _ => "yt_playback_error",
                };
                self.remote_active = None;
                self.events.push(PlaybackEvent::Error(Message::new(key)));
                if matches!(key, "yt_login_required" | "yt_auth_check_failed" | "yt_playback_interrupted") {
                    // Missing authentication is not a broken song: keep it selected so Play can retry.
                    self.set_state(PlaybackState::Stopped);
                } else {
                    self.error_after_remote();
                }
            }
            HelperEvent::Exited | HelperEvent::Closed => {
                if matches!(event, HelperEvent::Exited) {
                    self.remote = None;
                    self.remote_active = None;
                    self.events.push(PlaybackEvent::Error(Message::new("yt_browser_error")));
                    self.set_state(PlaybackState::Stopped);
                }
            }
            _ => {}
        }
    }

    fn error_after_remote(&mut self) {
        if let Some(current) = self.current() {
            self.failed.insert(current.to_string());
        }
        self.set_state(PlaybackState::Stopped);
        if self.queue.iter().any(|path| !self.failed.contains(path)) {
            self.next(true);
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> PlaybackState {
        self.last_state
    }

    /// The engine playing the current item, when it is not the local one.
    pub fn active_remote(&self) -> Option<Remote> {
        self.remote_active
    }

    pub fn playing(&self) -> bool {
        self.last_state == PlaybackState::Playing
    }

    pub fn position_ms(&self) -> u64 {
        self.snapshot.position_ms
    }

    pub fn duration_ms(&self) -> u64 {
        if self.snapshot.duration_ms > 0 {
            self.snapshot.duration_ms
        } else {
            (self.duration_hint * 1000.0) as u64
        }
    }

    pub fn set_duration_hint(&mut self, seconds: f64) {
        self.duration_hint = seconds;
    }

    pub fn start(&mut self, paths: Vec<String>, selected: &str) {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for path in paths {
            if seen.insert(path.clone()) {
                unique.push(path);
            }
        }
        let Some(index) = unique.iter().position(|p| p == selected) else { return };
        self.queue = unique;
        self.history.clear();
        self.failed.clear();
        self.index = Some(index);
        self.events.push(PlaybackEvent::QueueChanged);
        self.load();
    }

    /// Append missing tracks without changing playback; return the number added.
    pub fn enqueue(&mut self, paths: Vec<String>) -> usize {
        let existing: HashSet<String> = self.queue.iter().cloned().collect();
        let mut added = 0;
        let mut seen = HashSet::new();
        for path in paths {
            if !existing.contains(&path) && seen.insert(path.clone()) {
                self.queue.push(path);
                added += 1;
            }
        }
        if added > 0 {
            self.events.push(PlaybackEvent::QueueChanged);
        }
        added
    }

    /// Move requested tracks to the front in their given order and start playback.
    pub fn prepend_and_play(&mut self, paths: Vec<String>) {
        if let Some(first) = paths.first().cloned() {
            let mut all = paths;
            all.extend(self.queue.iter().cloned());
            self.start(all, &first);
        }
    }

    pub fn play_index(&mut self, index: usize) {
        if index < self.queue.len() {
            if let Some(current) = self.index {
                if current != index {
                    self.history.push(current);
                }
            }
            self.index = Some(index);
            let path = self.queue[index].clone();
            self.failed.remove(&path);
            self.load();
        }
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.index = None;
        self.history.clear();
        self.failed.clear();
        self.stop();
        self.snapshot = AudioSnapshot::default();
        self.events.push(PlaybackEvent::QueueChanged);
    }

    /// Move an occurrence while preserving the playing item and visit history.
    pub fn reorder_queue(&mut self, source: usize, target: usize) {
        if source == target || source >= self.queue.len() || target >= self.queue.len() {
            return;
        }
        let mut order: Vec<usize> = (0..self.queue.len()).collect();
        let moved = order.remove(source);
        order.insert(target, moved);
        let mut positions = vec![0usize; self.queue.len()];
        for (new, old) in order.iter().enumerate() {
            positions[*old] = new;
        }
        let old_queue = std::mem::take(&mut self.queue);
        self.queue = order.iter().map(|old| old_queue[*old].clone()).collect();
        self.index = self.index.map(|i| positions[i]);
        self.history = self.history.iter().filter(|i| **i < positions.len()).map(|i| positions[*i]).collect();
        self.events.push(PlaybackEvent::QueueChanged);
    }

    pub fn remove_from_queue(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        let current = self.index == Some(index);
        let state = self.last_state;
        let removed = self.queue.remove(index);
        self.history =
            self.history.iter().filter(|old| **old != index).map(|old| if *old > index { old - 1 } else { *old }).collect();
        if !self.queue.contains(&removed) {
            self.failed.remove(&removed);
        }
        if let Some(i) = self.index {
            if index < i {
                self.index = Some(i - 1);
            } else if current && index >= self.queue.len() {
                self.index = None;
            }
        }
        self.events.push(PlaybackEvent::QueueChanged);
        if current {
            if self.current().is_some() {
                self.load();
                if state == PlaybackState::Paused {
                    self.engine.pause();
                }
            } else {
                self.stop();
                self.snapshot = AudioSnapshot::default();
            }
        }
    }

    fn load(&mut self) {
        let Some(path) = self.current().map(str::to_string) else { return };
        self.events.push(PlaybackEvent::TrackChanged(path.clone()));
        self.set_state(PlaybackState::Stopped);
        if let Some(link) = self.active_link() {
            link.stop();
        }
        self.remote_active = None;
        if let Some(kind) = remote_kind(&path) {
            self.engine.stop();
            self.loaded_generation = 0;
            self.snapshot = AudioSnapshot::default();
            let id = match kind {
                Remote::YouTube => video_id(&path).ok(),
                Remote::Spotify => crate::spotify::track_id(&path).map(str::to_string),
            };
            let Some(link) = self.link(kind) else {
                // The YouTube Music engine starts on demand; Spotify's player exists once signed in.
                let event = match kind {
                    Remote::YouTube => PlaybackEvent::NeedsYouTube(path),
                    Remote::Spotify => PlaybackEvent::Error(Message::new("spotify_login_required")),
                };
                self.events.push(event);
                return;
            };
            let Some(id) = id else {
                self.error(Message::new(if kind == Remote::YouTube { "yt_playback_error" } else { "spotify_playback_error" }));
                return;
            };
            link.set_volume(self.volume as f32);
            link.set_muted(self.muted);
            let started = link.load(&id);
            if started {
                self.remote_active = Some(kind);
            } else {
                let key = if kind == Remote::YouTube { "yt_browser_error" } else { "spotify_sdk_failed" };
                self.events.push(PlaybackEvent::Error(Message::new(key)));
            }
            return;
        }
        let mut file = std::path::PathBuf::from(&path);
        if let Some(id) = crate::tidal::track_id(&path) {
            // TIDAL plays its 30-second preview, cached as a local file.
            let cached = self.preview_dir.as_deref().map(|dir| crate::tidal::preview::cache_file(dir, id));
            match cached.filter(|f| f.is_file()) {
                Some(cached) => {
                    file = cached;
                    self.duration_hint = self.duration_hint.min(30.0);
                }
                None => {
                    self.engine.stop();
                    self.loaded_generation = 0;
                    self.snapshot = AudioSnapshot::default();
                    self.events.push(PlaybackEvent::NeedsPreview(path));
                    return;
                }
            }
        }
        if !file.is_file() {
            self.engine.stop();
            let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(path.clone());
            self.error(Message::new("file_unavailable").with("path", name));
            return;
        }
        self.loaded_generation = self.engine.load(file, self.duration_hint);
        self.snapshot = self.engine.snapshot();
        self.set_state(PlaybackState::Playing);
    }

    /// Load the current item again (its TIDAL preview has arrived or the YouTube Music engine started).
    pub fn retry_current(&mut self) {
        self.load();
    }

    /// The current item's preview could not be fetched: report it and skip.
    pub fn preview_failed(&mut self, message: Message) {
        self.error(message);
    }

    pub fn toggle(&mut self) {
        if self.current().is_none() {
            return;
        }
        if let Some(kind) = self.current_remote() {
            let active = self.remote_active == Some(kind);
            match (self.link(kind), self.last_state) {
                (Some(link), PlaybackState::Playing) if active => {
                    link.pause();
                    self.set_state(PlaybackState::Paused);
                }
                (Some(link), PlaybackState::Paused) if active => {
                    link.play();
                    self.set_state(PlaybackState::Playing);
                }
                _ => self.load(),
            }
            return;
        }
        match self.last_state {
            PlaybackState::Playing => {
                self.engine.pause();
                self.set_state(PlaybackState::Paused);
            }
            PlaybackState::Paused => {
                self.engine.play();
                self.set_state(PlaybackState::Playing);
            }
            PlaybackState::Stopped => self.load(),
        }
    }

    pub fn next(&mut self, automatic: bool) {
        if self.queue.is_empty() {
            return;
        }
        let candidates: Vec<usize> = (0..self.queue.len()).filter(|i| !self.failed.contains(&self.queue[*i])).collect();
        if candidates.is_empty() {
            self.stop();
            return;
        }
        let target = if self.shuffle {
            let mut rng = rand::rng();
            let mut choices: Vec<usize> = candidates.iter().copied().filter(|i| Some(*i) != self.index).collect();
            let mut target = *choices.choose(&mut rng).or_else(|| candidates.choose(&mut rng)).unwrap();
            if automatic && !self.repeat {
                choices.retain(|i| !self.history.contains(i));
                if choices.is_empty() {
                    self.stop();
                    return;
                }
                target = *choices.choose(&mut rng).unwrap();
            }
            target
        } else {
            let following: Vec<usize> =
                candidates.iter().copied().filter(|i| self.index.map(|c| *i > c).unwrap_or(true)).collect();
            if following.is_empty() && automatic && !self.repeat {
                self.stop();
                return;
            }
            following.first().copied().unwrap_or(candidates[0])
        };
        if let Some(current) = self.index {
            self.history.push(current);
        }
        self.index = Some(target);
        self.load();
    }

    pub fn previous(&mut self) {
        if self.position_ms() > 3000 {
            self.seek(0);
        } else if !self.queue.is_empty() {
            self.index = Some(match self.history.pop() {
                Some(index) if index < self.queue.len() => index,
                _ => {
                    let current = self.index.unwrap_or(0);
                    (current + self.queue.len() - 1) % self.queue.len()
                }
            });
            self.load();
        }
    }

    pub fn stop(&mut self) {
        self.engine.stop();
        self.loaded_generation = 0;
        if let Some(link) = self.active_link() {
            link.stop();
        }
        self.remote_active = None;
        self.set_state(PlaybackState::Stopped);
    }

    pub fn seek(&mut self, ms: u64) {
        let ms = ms.min(self.duration_ms());
        match self.active_link() {
            Some(link) => link.seek(ms),
            None => self.engine.seek(ms),
        }
        self.snapshot.position_ms = ms;
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.volume = volume.clamp(0.0, 1.0);
        self.engine.set_volume(self.volume as f32);
        for kind in [Remote::YouTube, Remote::Spotify] {
            if let Some(link) = self.link(kind) {
                link.set_volume(self.volume as f32);
            }
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.engine.set_muted(muted);
        for kind in [Remote::YouTube, Remote::Spotify] {
            if let Some(link) = self.link(kind) {
                link.set_muted(muted);
            }
        }
    }

    /// Forget the helper (it exited or the user disconnected). Remote items stay queued.
    pub fn detach_remote(&mut self) {
        if self.remote_active == Some(Remote::YouTube) {
            self.remote_active = None;
            self.set_state(PlaybackState::Stopped);
        }
        self.remote = None;
    }

    /// Forget the Spotify player (signed out). Spotify items stay queued.
    pub fn detach_spotify(&mut self) {
        if self.remote_active == Some(Remote::Spotify) {
            if let Some(link) = &self.spotify {
                link.stop();
            }
            self.remote_active = None;
            self.set_state(PlaybackState::Stopped);
        }
        self.spotify = None;
    }

    fn set_state(&mut self, state: PlaybackState) {
        if self.last_state != state {
            self.last_state = state;
            self.events.push(PlaybackEvent::StateChanged(state));
        }
    }

    fn error(&mut self, message: Message) {
        self.events.push(PlaybackEvent::Error(message));
        if let Some(current) = self.current() {
            self.failed.insert(current.to_string());
        }
        self.set_state(PlaybackState::Stopped);
        // A broken folder must not create an endless error/skip loop.
        if self.queue.iter().any(|path| !self.failed.contains(path)) {
            self.next(true);
        }
    }

    /// Poll the audio engine. Call periodically from the UI timer.
    pub fn tick(&mut self) {
        if self.remote_active.is_some() {
            return;
        }
        if let Some(error) = self.engine.take_error() {
            self.error(Message::new("playback_error").with("message", error));
            return;
        }
        let snapshot = self.engine.snapshot();
        if snapshot.generation != self.loaded_generation || self.loaded_generation == 0 {
            return;
        }
        self.snapshot = snapshot.clone();
        if snapshot.ended {
            self.loaded_generation = 0;
            self.set_state(PlaybackState::Stopped);
            self.next(true);
            return;
        }
        self.set_state(snapshot.state);
    }

    pub fn take_events(&mut self) -> Vec<PlaybackEvent> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::player::{test_link, Command};

    const ONE: &str = "spotify:track:0DiWol3AO6WpXZgp0goxAV";
    const TWO: &str = "spotify:track:0DiWol3AO6WpXZgp0goxAW";

    fn errors(playback: &mut Playback) -> Vec<&'static str> {
        playback
            .take_events()
            .into_iter()
            .filter_map(|e| if let PlaybackEvent::Error(m) = e { Some(m.key) } else { None })
            .collect()
    }

    #[test]
    fn spotify_items_need_a_signed_in_player_and_are_kept_for_retry() {
        let mut playback = Playback::new();
        playback.start(vec![ONE.into(), TWO.into()], ONE);
        assert_eq!(errors(&mut playback), ["spotify_login_required"]);
        assert_eq!(playback.current(), Some(ONE));
        assert!(!playback.playing());
        assert_eq!(playback.current_remote(), Some(Remote::Spotify));
        assert_eq!(remote_kind("ytmusic:dQw4w9WgXcQ"), Some(Remote::YouTube));
        assert_eq!(remote_kind("/music/song.mp3"), None);
    }

    #[test]
    fn spotify_link_drives_playback_and_end_advances_the_queue() {
        let mut playback = Playback::new();
        let (link, commands) = test_link();
        playback.spotify = Some(link);
        playback.start(vec![ONE.into(), TWO.into()], ONE);
        let sent: Vec<Command> = commands.try_iter().collect();
        assert!(sent.contains(&Command::Load { uri: ONE.into() }), "{sent:?}");
        assert!(sent.contains(&Command::Volume(0.65)));

        playback.spotify_event(&PlayerEvent::State { playing: true, position_ms: 1500, duration_ms: 320_000 });
        assert!(playback.playing());
        assert_eq!((playback.position_ms(), playback.duration_ms()), (1500, 320_000));
        playback.toggle();
        assert_eq!(commands.try_iter().collect::<Vec<_>>(), [Command::Pause]);
        playback.seek(10_000);
        assert_eq!(commands.try_iter().collect::<Vec<_>>(), [Command::Seek(10_000)]);
        // Events of the other remote engine are ignored.
        playback.remote_event(&HelperEvent::Ended);
        assert_eq!(playback.current(), Some(ONE));

        playback.spotify_event(&PlayerEvent::Ended);
        assert_eq!(playback.current(), Some(TWO));
        let sent: Vec<Command> = commands.try_iter().collect();
        assert!(sent.contains(&Command::Load { uri: TWO.into() }), "{sent:?}");

        playback.spotify_event(&PlayerEvent::Error(Message::new("spotify_premium_required")));
        assert_eq!(playback.current(), Some(TWO), "account problems keep the item");
        playback.toggle();
        assert!(commands.try_iter().any(|c| c == Command::Load { uri: TWO.into() }), "play retries");
        playback.stop();
        assert!(commands.try_iter().any(|c| c == Command::Stop));
    }

    #[test]
    fn spotify_plays_on_this_computer_while_casting_and_a_broken_track_is_skipped() {
        let mut playback = Playback::new();
        let (link, commands) = test_link();
        playback.spotify = Some(link);
        playback.casting = true;
        playback.start(vec![ONE.into(), TWO.into()], ONE);
        assert_eq!(errors(&mut playback), Vec::<&str>::new(), "no error and no skip while casting");
        assert!(commands.try_iter().any(|c| c == Command::Load { uri: ONE.into() }));
        assert_eq!(playback.current(), Some(ONE));

        playback.casting = false;
        playback.start(vec![ONE.into(), TWO.into()], ONE);
        playback.spotify_event(&PlayerEvent::Error(Message::new("spotify_playback_error")));
        assert_eq!(playback.current(), Some(TWO));
        playback.detach_spotify();
        assert!(playback.spotify.is_none() && !playback.playing());
    }

    #[test]
    fn a_youtube_item_without_a_running_engine_asks_for_it_and_keeps_its_place() {
        let mut playback = Playback::new();
        let (link, _commands) = test_link();
        playback.spotify = Some(link);
        let video = "ytmusic:dQw4w9WgXcQ";
        playback.start(vec![ONE.into(), video.into()], ONE);
        playback.take_events();
        playback.spotify_event(&PlayerEvent::Ended);
        let events = playback.take_events();
        assert!(events.iter().any(|e| matches!(e, PlaybackEvent::NeedsYouTube(p) if p == video)), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, PlaybackEvent::Error(_))), "{events:?}");
        assert_eq!(playback.current(), Some(video), "the item stays current for the retry");
        assert!(!playback.playing());
    }
}
