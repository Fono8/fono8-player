//! One-shot playback deadline, kept independently of playback and window visibility.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct SleepTimer {
    deadline: Option<f64>,
    remaining: u64,
    started: Option<Instant>,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl SleepTimer {
    pub fn active(&self) -> bool {
        self.deadline.is_some()
    }

    #[allow(dead_code)]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    pub fn start(&mut self, minutes: u64) {
        if !(1..=24 * 60).contains(&minutes) {
            return;
        }
        // A wall-clock deadline includes time spent with the computer suspended.
        self.deadline = Some(now() + (minutes * 60) as f64);
        self.remaining = minutes * 60;
        self.started = Some(Instant::now());
    }

    pub fn cancel(&mut self) {
        self.deadline = None;
        self.remaining = 0;
        self.started = None;
    }

    /// Advance the timer; returns `true` when the deadline has just expired.
    pub fn tick(&mut self) -> bool {
        let Some(deadline) = self.deadline else { return false };
        let remaining = (deadline - now()).ceil().max(0.0) as u64;
        if remaining == 0 {
            self.cancel();
            return true;
        }
        self.remaining = remaining;
        false
    }

    pub fn countdown(&self) -> String {
        let seconds = self.remaining;
        let hours = seconds / 3600;
        let minutes = (seconds / 60) % 60;
        if hours > 0 {
            format!("{hours}:{minutes:02}:{:02}", seconds % 60)
        } else {
            format!("{minutes}:{:02}", seconds % 60)
        }
    }

    #[allow(dead_code)]
    pub fn elapsed(&self) -> Duration {
        self.started.map(|s| s.elapsed()).unwrap_or_default()
    }
}
