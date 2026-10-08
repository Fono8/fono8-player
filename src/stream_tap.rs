//! Band levels of audio Fono8 does not decode itself: the Spotify player's output
//! stream, read from PipeWire with `pw-record` while Spotify is the active source
//! (Linux). Spotify's audio is encrypted inside the browser, so the logo can only
//! measure what the browser hands to the system. The samples only feed the level
//! meter; nothing is stored or sent anywhere.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::meter::{Analyzer, Levels};

const RATE: u32 = 44_100;
/// 1024 stereo frames of f32 (~23 ms), one meter window.
const CHUNK_BYTES: usize = 1024 * 2 * 4;

/// Follows one browser's output stream until dropped.
pub struct StreamTap {
    stop: Arc<AtomicBool>,
    recorder: Arc<Mutex<Option<Child>>>,
}

impl StreamTap {
    /// Measure the audio of the browser started with `--user-data-dir=<profile>`.
    pub fn start(profile: &Path, levels: Levels) -> StreamTap {
        let stop = Arc::new(AtomicBool::new(false));
        let recorder = Arc::new(Mutex::new(None));
        let marker = format!("--user-data-dir={}", profile.display());
        let (thread_stop, thread_recorder) = (stop.clone(), recorder.clone());
        let _ = std::thread::Builder::new()
            .name("fono8-stream-tap".into())
            .spawn(move || follow(&marker, &levels, &thread_stop, &thread_recorder));
        StreamTap { stop, recorder }
    }
}

impl Drop for StreamTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut recorder) = self.recorder.lock() {
            if let Some(child) = recorder.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

fn follow(marker: &str, levels: &Levels, stop: &AtomicBool, slot: &Mutex<Option<Child>>) {
    while !stop.load(Ordering::Relaxed) {
        // The stream appears once the browser starts playing and may be recreated later.
        let Some(serial) = find_stream(marker) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let spawned = Command::new("pw-record")
            .args(["--raw", "--target", &serial, "--format", "f32", "--channels", "2", "--latency", "20ms"])
            .args(["--rate", &RATE.to_string(), "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                crate::app::debug(|| format!("stream tap: pw-record unavailable: {error}"));
                return;
            }
        };
        let Some(mut stdout) = child.stdout.take() else { return };
        if let Ok(mut recorder) = slot.lock() {
            *recorder = Some(child);
        }
        crate::app::debug(|| format!("stream tap: measuring PipeWire stream {serial}"));
        let mut analyzer = Analyzer::new(levels.clone(), RATE);
        let mut bytes = vec![0u8; CHUNK_BYTES];
        let mut samples = Vec::with_capacity(CHUNK_BYTES / 4);
        while !stop.load(Ordering::Relaxed) && stdout.read_exact(&mut bytes).is_ok() {
            samples.clear();
            samples.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            analyzer.feed(&samples);
        }
        if let Some(mut child) = slot.lock().ok().and_then(|mut recorder| recorder.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The `object.serial` of the audio output stream whose process belongs to the browser
/// started with `marker` (Chromium's audio service carries `--user-data-dir` too).
fn find_stream(marker: &str) -> Option<String> {
    let output = Command::new("pw-dump").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let objects: Value = serde_json::from_slice(&output.stdout).ok()?;
    pick_stream(&objects, |pid| std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| has_arg(&cmdline, marker)))
}

/// Whether a `/proc/<pid>/cmdline` holds `arg`. Chromium rewrites its process title into
/// one space-separated string, so a match may end in a space as well as a NUL.
fn has_arg(cmdline: &[u8], arg: &str) -> bool {
    let arg = arg.as_bytes();
    cmdline.windows(arg.len()).enumerate().any(|(start, window)| {
        let before = start.checked_sub(1).map(|i| cmdline[i]);
        let after = cmdline.get(start + arg.len()).copied();
        window == arg && matches!(before, None | Some(0 | b' ')) && matches!(after, None | Some(0 | b' '))
    })
}

fn pick_stream(objects: &Value, belongs: impl Fn(u32) -> bool) -> Option<String> {
    let text = |value: &Value| match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    objects.as_array()?.iter().find_map(|object| {
        let props = object.get("info")?.get("props")?;
        if props.get("media.class")?.as_str()? != "Stream/Output/Audio" {
            return None;
        }
        let pid: u32 = text(props.get("application.process.id")?)?.parse().ok()?;
        belongs(pid).then(|| text(props.get("object.serial")?)).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_the_output_stream_of_the_right_process() {
        let objects = json!([
            {"id": 1, "info": {"props": {"media.class": "Audio/Sink", "object.serial": 10}}},
            {"id": 2, "info": {"props": {"media.class": "Stream/Output/Audio", "object.serial": 1408, "application.process.id": "710507"}}},
            {"id": 3, "info": {"props": {"media.class": "Stream/Input/Audio", "object.serial": 1500, "application.process.id": "712619"}}},
            {"id": 4, "info": {"props": {"media.class": "Stream/Output/Audio", "object.serial": 1424, "application.process.id": "712619"}}},
            {"id": 5, "info": null}
        ]);
        assert_eq!(pick_stream(&objects, |pid| pid == 712619).as_deref(), Some("1424"));
        assert_eq!(pick_stream(&objects, |pid| pid == 1), None);
    }

    #[test]
    fn finds_the_profile_in_plain_and_rewritten_command_lines() {
        let marker = "--user-data-dir=/home/me/fono8/spotify-profile";
        assert!(has_arg(b"chrome\0--type=utility\0--user-data-dir=/home/me/fono8/spotify-profile\0", marker));
        assert!(has_arg(b"chrome --type=utility --user-data-dir=/home/me/fono8/spotify-profile --lang=en", marker));
        assert!(!has_arg(b"chrome --user-data-dir=/home/me/fono8/spotify-profile-old --lang=en", marker));
        assert!(!has_arg(b"chrome --user-data-dir=/home/me/fono8/ytmusic-profile", marker));
    }

    /// Live check against a running browser:
    /// `FONO8_TAP_PROFILE=~/.local/share/fono8/spotify-profile cargo test stream_tap -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measures_a_running_browser() {
        let Some(profile) = std::env::var_os("FONO8_TAP_PROFILE") else { return };
        let marker = format!("--user-data-dir={}", Path::new(&profile).display());
        eprintln!("stream: {:?}", find_stream(&marker));
        let levels = Levels::default();
        let tap = StreamTap::start(Path::new(&profile), levels.clone());
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(200));
            let (updates, bars) = levels.read();
            eprintln!("{updates:4} {bars:.2?}");
        }
        drop(tap);
    }
}
