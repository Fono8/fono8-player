//! Live loudness of five frequency bands, so the logo bars move with the music.
//!
//! The audio thread feeds the decoded PCM into an [`Analyzer`] (five band-pass
//! filters, a few multiplications per sample) that publishes one value per band
//! about 43 times a second through atomics. The UI and the tray read them without
//! locking and smooth them with [`Motion`]. YouTube Music measures in its page and
//! sends dB values that go through the same scaling ([`Meter`]); Spotify publishes
//! nothing (its audio is encrypted), and the logo shows a generic [`wave`].

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BANDS: usize = 5;
/// Band centres in Hz, bass on the left.
const CENTRES: [f32; BANDS] = [90.0, 280.0, 900.0, 2800.0, 8000.0];
const Q: f32 = 1.1;
/// Frames per published value (~23 ms at 44.1 kHz).
const WINDOW: usize = 1024;
/// Each band is drawn relative to its own recent average and peak, so beats stand
/// out in compressed recordings and quiet bands still move. Time constants are in
/// windows: the average follows over ~1.5 s, the peak falls ~6 dB/s.
const AVERAGE_FOLLOW: f32 = 0.015;
const PEAK_FALL_DB: f32 = 0.14;
/// Where the average sits on the bar, and the smallest average-to-peak span.
const BELOW_AVERAGE_DB: f32 = 4.0;
const MIN_SPAN_DB: f32 = 9.0;
/// Bands this far below the loudest band's peak fade out (near-silence, empty treble).
const GATE_DB: f32 = 38.0;
const GATE_SOFT_DB: f32 = 10.0;
/// Without a new value for this long the meter counts as stopped (paused, other source).
const STALE: Duration = Duration::from_millis(300);

/// The latest band levels (0..1), shared between the audio thread and the UI.
#[derive(Clone, Default)]
pub struct Levels(Arc<Shared>);

#[derive(Default)]
struct Shared {
    bands: [AtomicU32; BANDS],
    updates: AtomicU64,
    /// Averages and peaks of the last track, so the next one starts calibrated.
    memory: Mutex<Option<Memory>>,
}

#[derive(Clone, Copy)]
struct Memory {
    average: [f32; BANDS],
    peak: [f32; BANDS],
}

impl Default for Memory {
    fn default() -> Memory {
        Memory { average: [-30.0; BANDS], peak: [-20.0; BANDS] }
    }
}

impl Levels {
    fn publish(&self, levels: &[f32; BANDS]) {
        for (slot, level) in self.0.bands.iter().zip(levels) {
            slot.store(level.to_bits(), Ordering::Relaxed);
        }
        self.0.updates.fetch_add(1, Ordering::Release);
    }

    /// A counter that changes with every publish, and the band levels.
    pub fn read(&self) -> (u64, [f32; BANDS]) {
        let updates = self.0.updates.load(Ordering::Acquire);
        (updates, std::array::from_fn(|i| f32::from_bits(self.0.bands[i].load(Ordering::Relaxed))))
    }

    fn memory(&self) -> Memory {
        self.0.memory.lock().ok().and_then(|m| *m).unwrap_or_default()
    }
}

/// RBJ band-pass (0 dB peak), transposed direct form II.
#[derive(Clone, Copy)]
struct BandPass {
    b0: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl BandPass {
    fn new(rate: f32, centre: f32) -> BandPass {
        let w0 = std::f32::consts::TAU * centre.min(rate * 0.45) / rate;
        let alpha = w0.sin() / (2.0 * Q);
        let a0 = 1.0 + alpha;
        BandPass { b0: alpha / a0, a1: -2.0 * w0.cos() / a0, a2: (1.0 - alpha) / a0, z1: 0.0, z2: 0.0 }
    }

    /// b1 = 0 and b2 = -b0 for this filter.
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.z2 - self.a1 * y;
        self.z2 = -self.b0 * x - self.a2 * y;
        y
    }
}

/// Turns band loudness (dB) into bar levels: each band relative to its own recent
/// average and peak, faded out far below the loudest band.
#[derive(Default)]
struct Normalizer {
    memory: Memory,
}

impl Normalizer {
    fn levels(&mut self, db: &[f32; BANDS]) -> [f32; BANDS] {
        let Memory { average, peak } = &mut self.memory;
        for i in 0..BANDS {
            average[i] += (db[i] - average[i]) * AVERAGE_FOLLOW;
            peak[i] = db[i].max(peak[i] - PEAK_FALL_DB).max(average[i]);
        }
        let loudest = peak.iter().copied().fold(f32::MIN, f32::max);
        std::array::from_fn(|i| {
            let floor = average[i] - BELOW_AVERAGE_DB;
            let span = (peak[i] - floor).max(MIN_SPAN_DB);
            let gate = ((db[i] - (loudest - GATE_DB)) / GATE_SOFT_DB).clamp(0.0, 1.0);
            ((db[i] - floor) / span).clamp(0.0, 1.0) * gate
        })
    }
}

/// Band loudness measured elsewhere (the YouTube Music page), published as bar levels.
pub struct Meter {
    levels: Levels,
    normalizer: Normalizer,
}

impl Meter {
    pub fn new(levels: Levels) -> Meter {
        Meter { levels, normalizer: Normalizer::default() }
    }

    /// `db`: five bands, bass first; anything else is ignored.
    pub fn push(&mut self, db: &[f32]) {
        let Ok(db) = <[f32; BANDS]>::try_from(db) else { return };
        if db.iter().all(|v| v.is_finite()) {
            let levels = self.normalizer.levels(&db);
            self.levels.publish(&levels);
        }
    }
}

/// Measures interleaved stereo PCM on the audio thread.
pub struct Analyzer {
    levels: Levels,
    filters: [BandPass; BANDS],
    energy: [f32; BANDS],
    normalizer: Normalizer,
    frames: usize,
}

impl Analyzer {
    pub fn new(levels: Levels, rate: u32) -> Analyzer {
        let memory = levels.memory();
        Analyzer {
            levels,
            filters: CENTRES.map(|centre| BandPass::new(rate as f32, centre)),
            energy: [0.0; BANDS],
            normalizer: Normalizer { memory },
            frames: 0,
        }
    }

    pub fn feed(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            let x = (frame[0] + frame[1]) * 0.5;
            for (filter, energy) in self.filters.iter_mut().zip(self.energy.iter_mut()) {
                let y = filter.run(x);
                *energy += y * y;
            }
            self.frames += 1;
            if self.frames == WINDOW {
                self.publish();
            }
        }
    }

    fn publish(&mut self) {
        let db: [f32; BANDS] = std::array::from_fn(|i| 10.0 * (self.energy[i] / self.frames as f32 + 1e-12).log10());
        let levels = self.normalizer.levels(&db);
        for filter in &mut self.filters {
            // Silence leaves denormal filter state behind, which is slow on x86.
            if filter.z1.abs() < 1e-20 && filter.z2.abs() < 1e-20 {
                (filter.z1, filter.z2) = (0.0, 0.0);
            }
        }
        self.energy = [0.0; BANDS];
        self.frames = 0;
        self.levels.publish(&levels);
    }
}

impl Drop for Analyzer {
    fn drop(&mut self) {
        if let Ok(mut memory) = self.levels.0.memory.lock() {
            *memory = Some(self.normalizer.memory);
        }
    }
}

/// Bar motion for drawing: a quick rise and a slower fall, independent of the frame rate.
#[derive(Default)]
pub struct Motion {
    bars: [f32; BANDS],
    updates: u64,
    fresh: Option<Instant>,
    last: Option<Instant>,
}

impl Motion {
    /// Whether `levels` received a value recently, i.e. the music is being measured now.
    pub fn live(&mut self, levels: &Levels, now: Instant) -> bool {
        let (updates, _) = levels.read();
        if updates != self.updates {
            self.updates = updates;
            self.fresh = Some(now);
        }
        self.fresh.is_some_and(|fresh| now.duration_since(fresh) < STALE)
    }

    /// Advance to `now`; `None` while the meter is not live.
    pub fn step(&mut self, levels: &Levels, now: Instant) -> Option<[f32; BANDS]> {
        if !self.live(levels, now) {
            self.bars = [0.0; BANDS];
            self.last = None;
            return None;
        }
        let dt = self.last.map_or(0.033, |last| now.duration_since(last).as_secs_f32().min(0.1));
        self.last = Some(now);
        let (_, target) = levels.read();
        let rise = 1.0 - (-dt / 0.035).exp();
        let fall = 1.0 - (-dt / 0.14).exp();
        for (bar, target) in self.bars.iter_mut().zip(target) {
            *bar += (target - *bar) * if target > *bar { rise } else { fall };
        }
        Some(self.bars)
    }
}

/// The generic wave for sources that cannot be measured: each bar bounces with its own period.
pub fn wave(elapsed: Duration) -> [f32; BANDS] {
    let ms = elapsed.as_millis() as f32;
    std::array::from_fn(|i| {
        let period = 900.0 + i as f32 * 140.0;
        // Up and back down, eased at both ends.
        let t = (ms % period) / period;
        let x = 1.0 - (2.0 * t - 1.0).abs();
        let eased = x * x * (3.0 - 2.0 * x);
        0.1 + 0.85 * eased
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let v = amplitude * (std::f32::consts::TAU * freq * n as f32 / 44_100.0).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn bass_and_treble_reach_only_their_side() {
        for (band, other, freq) in [(0, 4, 90.0), (4, 0, 8000.0)] {
            let levels = Levels::default();
            let mut analyzer = Analyzer::new(levels.clone(), 44_100);
            analyzer.feed(&tone(freq, 0.5, WINDOW * 20));
            let (updates, bars) = levels.read();
            assert_eq!(updates, 20);
            assert!(bars[band] > 0.8 && bars[other] < 0.1, "{freq} Hz: {bars:?}");
        }
    }

    #[test]
    fn a_beat_stands_out_from_the_average() {
        let levels = Levels::default();
        let mut analyzer = Analyzer::new(levels.clone(), 44_100);
        for _ in 0..8 {
            analyzer.feed(&tone(90.0, 0.05, WINDOW * 10));
            analyzer.feed(&tone(90.0, 0.5, WINDOW));
            let beat = levels.read().1[0];
            analyzer.feed(&tone(90.0, 0.05, WINDOW * 2));
            let after = levels.read().1[0];
            assert!(beat > 0.9 && after < 0.6, "beat {beat}, after {after}");
        }
    }

    #[test]
    fn silence_is_zero_and_ceilings_carry_over() {
        let levels = Levels::default();
        let mut analyzer = Analyzer::new(levels.clone(), 44_100);
        analyzer.feed(&tone(280.0, 0.3, WINDOW * 4));
        analyzer.feed(&vec![0.0; WINDOW * 2 * 4]);
        assert_eq!(levels.read().1, [0.0; BANDS]);
        drop(analyzer);
        let carried = Analyzer::new(levels.clone(), 44_100).normalizer.memory;
        assert!(carried.peak[1] > -20.0 && carried.average[1] < carried.peak[1], "{:?}", carried.peak);
    }

    #[test]
    fn motion_follows_live_levels_and_stops_when_stale() {
        let levels = Levels::default();
        let mut motion = Motion::default();
        let start = Instant::now();
        assert_eq!(motion.step(&levels, start), None, "nothing measured yet");
        levels.publish(&[1.0; BANDS]);
        let first = motion.step(&levels, start).unwrap();
        assert!(first[0] > 0.0 && first[0] < 1.0, "rises over a few frames: {first:?}");
        let later = motion.step(&levels, start + Duration::from_millis(100)).unwrap();
        assert!(later[0] > first[0]);
        levels.publish(&[0.0; BANDS]);
        let falling = motion.step(&levels, start + Duration::from_millis(133)).unwrap();
        assert!(falling[0] < later[0] && falling[0] > 0.5, "falls slower than it rises: {falling:?}");
        assert_eq!(motion.step(&levels, start + Duration::from_millis(500)), None, "paused");
    }

    #[test]
    fn meter_scales_page_measurements() {
        let levels = Levels::default();
        let mut meter = Meter::new(levels.clone());
        meter.push(&[-40.0; 3]);
        assert_eq!(levels.read().0, 0, "wrong band count is ignored");
        for _ in 0..50 {
            meter.push(&[-30.0, -35.0, -40.0, -45.0, -50.0]);
        }
        meter.push(&[-15.0, -35.0, -40.0, -45.0, f32::NEG_INFINITY]);
        assert_eq!(levels.read().0, 50, "non-finite values are skipped");
        meter.push(&[-15.0, -35.0, -40.0, -45.0, -50.0]);
        let (_, bars) = levels.read();
        assert!(bars[0] > 0.9 && bars[1] < bars[0], "{bars:?}");
    }

    #[test]
    fn wave_moves_within_range() {
        let a = wave(Duration::from_millis(100));
        let b = wave(Duration::from_millis(400));
        assert_ne!(a, b);
        assert!(a.iter().chain(&b).all(|v| (0.0..=1.0).contains(v)));
    }

    /// Tuning aid: `FONO8_METER_FILE=song.flac cargo test meter -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn report_for_a_file() {
        use rodio::Source;
        let Some(path) = std::env::var_os("FONO8_METER_FILE") else { return };
        let decoder = rodio::Decoder::try_from(std::fs::File::open(path).unwrap()).unwrap();
        let samples: Vec<f32> = rodio::source::UniformSourceIterator::new(
            decoder.take_duration(Duration::from_secs(60)),
            std::num::NonZero::new(2).unwrap(),
            std::num::NonZero::new(44_100).unwrap(),
        )
        .collect();
        let levels = Levels::default();
        let mut analyzer = Analyzer::new(levels.clone(), 44_100);
        let mut motion = Motion::default();
        let start = Instant::now();
        let mut frames: Vec<[f32; BANDS]> = Vec::new();
        // 30 frames per second of drawing: 1470 stereo frames of audio each.
        for (n, chunk) in samples.chunks(1470 * 2).enumerate() {
            analyzer.feed(chunk);
            frames.push(motion.step(&levels, start + Duration::from_millis(n as u64 * 1000 / 30)).unwrap_or_default());
        }
        for band in 0..BANDS {
            let values: Vec<f32> = frames.iter().map(|f| f[band]).collect();
            let mean = values.iter().sum::<f32>() / values.len() as f32;
            let movement = values.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / values.len() as f32;
            let high = values.iter().filter(|v| **v > 0.85).count() as f32 / values.len() as f32;
            let low = values.iter().filter(|v| **v < 0.1).count() as f32 / values.len() as f32;
            eprintln!("band {band}: mean {mean:.2} movement/frame {movement:.3} >0.85 {high:.2} <0.1 {low:.2}");
        }
        for frame in frames.iter().skip(300).take(60) {
            let bars: String = frame.iter().map(|v| ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][(v * 7.99) as usize]).collect();
            eprint!("{bars} ");
        }
        eprintln!();
    }
}
