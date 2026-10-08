//! Audio output on a dedicated thread (rodio + symphonia).
//!
//! The UI never touches the device directly: it sends commands and reads a
//! snapshot of the engine state on each tick.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::meter::{Analyzer, Levels};
use rodio::source::{Source, UniformSourceIterator};
use std::sync::mpsc::SyncSender;

/// Sample rate and channel count of the PCM delivered to a capture sink.
pub const CAPTURE_RATE: u32 = 44_100;
pub const CAPTURE_CHANNELS: u16 = 2;

/// Where a copy of the decoded audio goes while casting (interleaved f32, 44.1 kHz stereo).
pub type CaptureSink = Arc<Mutex<Option<SyncSender<Vec<f32>>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug)]
pub struct AudioSnapshot {
    pub generation: u64,
    pub state: PlaybackState,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Set when the current source has played to its end; cleared by the next load.
    pub ended: bool,
    /// A decode/output error for the current generation, reported once.
    pub error: Option<String>,
}

impl Default for AudioSnapshot {
    fn default() -> Self {
        Self { generation: 0, state: PlaybackState::Stopped, position_ms: 0, duration_ms: 0, ended: false, error: None }
    }
}

enum Command {
    Load { path: PathBuf, generation: u64, duration_hint: f64 },
    Play,
    Pause,
    Stop,
    Seek(u64),
    Volume(f32),
    Muted(bool),
    Quit,
}

pub struct AudioEngine {
    sender: Sender<Command>,
    state: Arc<Mutex<AudioSnapshot>>,
    generation: u64,
    capture: CaptureSink,
    levels: Levels,
}

impl AudioEngine {
    pub fn start() -> AudioEngine {
        let (sender, receiver) = mpsc::channel::<Command>();
        let state = Arc::new(Mutex::new(AudioSnapshot::default()));
        let capture: CaptureSink = Arc::new(Mutex::new(None));
        let shared = state.clone();
        let tap = capture.clone();
        let levels = Levels::default();
        let meter = levels.clone();
        std::thread::Builder::new()
            .name("fono8-audio".into())
            .spawn(move || audio_thread(receiver, shared, tap, meter))
            .expect("audio thread");
        AudioEngine { sender, state, generation: 0, capture, levels }
    }

    /// Band levels of what is playing, for the animated logo.
    pub fn levels(&self) -> Levels {
        self.levels.clone()
    }

    /// The sink that receives decoded PCM while casting; shared with the Cast worker.
    /// Local output is silenced for as long as a sender is attached.
    pub fn capture_sink(&self) -> CaptureSink {
        self.capture.clone()
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn take_error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|mut s| s.error.take())
    }

    /// Load a file and start playing it. Returns the generation of this load.
    pub fn load(&mut self, path: PathBuf, duration_hint: f64) -> u64 {
        self.generation += 1;
        if let Ok(mut state) = self.state.lock() {
            *state = AudioSnapshot {
                generation: self.generation,
                state: PlaybackState::Playing,
                duration_ms: (duration_hint * 1000.0) as u64,
                ..Default::default()
            };
        }
        let _ = self.sender.send(Command::Load { path, generation: self.generation, duration_hint });
        self.generation
    }

    pub fn play(&self) {
        let _ = self.sender.send(Command::Play);
    }

    pub fn pause(&self) {
        let _ = self.sender.send(Command::Pause);
    }

    pub fn stop(&mut self) {
        self.generation += 1;
        if let Ok(mut state) = self.state.lock() {
            *state = AudioSnapshot { generation: self.generation, ..Default::default() };
        }
        let _ = self.sender.send(Command::Stop);
    }

    pub fn seek(&self, ms: u64) {
        let _ = self.sender.send(Command::Seek(ms));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.sender.send(Command::Volume(volume));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.sender.send(Command::Muted(muted));
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Quit);
    }
}

struct Output {
    _sink: rodio::MixerDeviceSink,
    player: rodio::Player,
}

fn open_output() -> Result<Output, String> {
    let sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
    let player = rodio::Player::connect_new(sink.mixer());
    Ok(Output { _sink: sink, player })
}

/// Copies every sample to the capture sink (when attached) and the level meter
/// before it reaches the output.
struct Tee<S> {
    inner: S,
    sink: CaptureSink,
    meter: Analyzer,
    pending: Vec<f32>,
}

impl<S: Source> Tee<S> {
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.meter.feed(&self.pending);
        let chunk = std::mem::take(&mut self.pending);
        if let Ok(slot) = self.sink.lock() {
            if let Some(sender) = slot.as_ref() {
                // A stalled encoder must never block the audio thread.
                let _ = sender.try_send(chunk);
            }
        }
    }
}

impl<S: Source> Iterator for Tee<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next();
        match sample {
            Some(value) => {
                self.pending.push(value);
                if self.pending.len() >= 2048 {
                    self.flush();
                }
            }
            None => self.flush(),
        }
        sample
    }
}

impl<S: Source> Source for Tee<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.pending.clear();
        self.inner.try_seek(pos)
    }
}

fn audio_thread(receiver: mpsc::Receiver<Command>, state: Arc<Mutex<AudioSnapshot>>, capture: CaptureSink, levels: Levels) {
    let mut output: Option<Output> = None;
    let mut volume: f32 = 0.65;
    let mut muted = false;
    let mut capturing = false;
    let effective = |volume: f32, muted: bool, capturing: bool| if muted || capturing { 0.0 } else { volume };
    let mut generation: u64 = 0;
    let mut loaded = false;
    let mut duration_ms: u64 = 0;
    let mut position_offset = Duration::ZERO;
    loop {
        let command = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match command {
            Some(Command::Quit) => return,
            Some(Command::Load { path, generation: new_generation, duration_hint }) => {
                generation = new_generation;
                loaded = false;
                position_offset = Duration::ZERO;
                duration_ms = (duration_hint * 1000.0) as u64;
                if output.is_none() {
                    match open_output() {
                        Ok(opened) => output = Some(opened),
                        Err(error) => {
                            report_error(&state, generation, error);
                            continue;
                        }
                    }
                }
                let Some(out) = output.as_mut() else { continue };
                out.player.stop();
                // `stop` leaves the player paused; recreate it so controls start clean.
                out.player = rodio::Player::connect_new(out._sink.mixer());
                out.player.set_volume(effective(volume, muted, capturing));
                match open_source(&path) {
                    Ok((source, duration)) => {
                        if let Some(duration) = duration {
                            duration_ms = duration.as_millis() as u64;
                        }
                        let uniform = UniformSourceIterator::new(
                            source,
                            std::num::NonZero::new(CAPTURE_CHANNELS).unwrap(),
                            std::num::NonZero::new(CAPTURE_RATE).unwrap(),
                        );
                        out.player.append(Tee {
                            inner: uniform,
                            sink: capture.clone(),
                            meter: Analyzer::new(levels.clone(), CAPTURE_RATE),
                            pending: Vec::with_capacity(2048),
                        });
                        out.player.play();
                        loaded = true;
                        if let Ok(mut s) = state.lock() {
                            if s.generation == generation {
                                s.state = PlaybackState::Playing;
                                s.duration_ms = duration_ms;
                                s.position_ms = 0;
                                s.ended = false;
                            }
                        }
                    }
                    Err(error) => report_error(&state, generation, error),
                }
            }
            Some(Command::Play) => {
                if let Some(out) = output.as_ref() {
                    if loaded {
                        out.player.play();
                    }
                }
            }
            Some(Command::Pause) => {
                if let Some(out) = output.as_ref() {
                    out.player.pause();
                }
            }
            Some(Command::Stop) => {
                if let Some(out) = output.as_mut() {
                    out.player.stop();
                }
                loaded = false;
            }
            Some(Command::Seek(ms)) => {
                if let Some(out) = output.as_ref() {
                    if loaded {
                        let target = Duration::from_millis(ms.min(duration_ms.max(ms)));
                        match out.player.try_seek(target) {
                            Ok(()) => position_offset = Duration::ZERO,
                            Err(_) => {}
                        }
                        if let Ok(mut s) = state.lock() {
                            s.position_ms = ms;
                        }
                    }
                }
            }
            Some(Command::Volume(value)) => {
                volume = value.clamp(0.0, 1.0);
                if let Some(out) = output.as_ref() {
                    out.player.set_volume(effective(volume, muted, capturing));
                }
            }
            Some(Command::Muted(value)) => {
                muted = value;
                if let Some(out) = output.as_ref() {
                    out.player.set_volume(effective(volume, muted, capturing));
                }
            }
            None => {}
        }
        // Silence local output while the Cast relay consumes the capture tap.
        let capture_attached = capture.lock().map(|slot| slot.is_some()).unwrap_or(false);
        if capture_attached != capturing {
            capturing = capture_attached;
            if let Some(out) = output.as_ref() {
                out.player.set_volume(effective(volume, muted, capturing));
            }
        }
        // Refresh the snapshot for the UI.
        if let (Some(out), true) = (output.as_ref(), loaded) {
            let empty = out.player.empty();
            let paused = out.player.is_paused();
            let position = out.player.get_pos() + position_offset;
            if let Ok(mut s) = state.lock() {
                if s.generation == generation {
                    if empty {
                        s.state = PlaybackState::Stopped;
                        s.position_ms = duration_ms;
                        s.ended = true;
                    } else {
                        s.state = if paused { PlaybackState::Paused } else { PlaybackState::Playing };
                        s.position_ms = (position.as_millis() as u64).min(duration_ms.max(position.as_millis() as u64));
                        s.duration_ms = duration_ms;
                    }
                }
            }
            if empty {
                loaded = false;
            }
        }
    }
}

fn report_error(state: &Arc<Mutex<AudioSnapshot>>, generation: u64, error: String) {
    if let Ok(mut s) = state.lock() {
        if s.generation == generation {
            s.state = PlaybackState::Stopped;
            s.error = Some(error);
        }
    }
}

fn open_source(path: &PathBuf) -> Result<(rodio::Decoder<BufReader<File>>, Option<Duration>), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let decoder = rodio::Decoder::builder()
        .with_data(BufReader::new(file))
        .with_byte_len(len)
        .with_seekable(true)
        .with_gapless(true)
        .build()
        .map_err(|e| e.to_string())?;
    let duration = decoder.total_duration();
    Ok((decoder, duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an audio device; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn plays_a_wav_file_to_the_end() {
        let dir = std::env::temp_dir().join(format!("fono8-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        crate::library::tests::write_wav(&path, 1);
        let mut engine = AudioEngine::start();
        let generation = engine.load(path, 1.0);
        let mut saw_playing = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            let snapshot = engine.snapshot();
            assert_eq!(snapshot.generation, generation);
            assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
            if snapshot.state == PlaybackState::Playing {
                saw_playing = true;
            }
            if snapshot.ended {
                assert!(saw_playing);
                assert!(snapshot.duration_ms >= 900 && snapshot.duration_ms <= 1100, "{}", snapshot.duration_ms);
                return;
            }
        }
        panic!("playback did not finish: {:?}", engine.snapshot());
    }
}
