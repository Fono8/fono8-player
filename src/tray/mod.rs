//! System tray icon with the Fono8 bars that move with the music while it plays.
//!
//! Linux uses a StatusNotifierItem over D-Bus (`ksni`, no GTK needed); macOS and
//! Windows use the native status bar / notification area through `tray-icon`.
//! Both expose the same `Tray` API, polled by the model on each tick.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;

#[cfg(target_os = "linux")]
pub use linux::Tray;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use native::Tray;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    PlayPause,
    Previous,
    Next,
    Quit,
}

pub struct TrayTexts {
    pub tooltip: String,
    pub show: String,
    pub song: String,
    pub play_pause: String,
    pub previous: String,
    pub next: String,
    pub quit: String,
}

const ACCENT: (u8, u8, u8) = (0x3d, 0xe8, 0xf7);
const PURPLE: (u8, u8, u8) = (0x9b, 0x6d, 0xff);

/// How the tray logo is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pose {
    Still,
    /// A generic wave, `phase` in 0..1, for sources Fono8 does not decode itself.
    Wave(f32),
    /// Band levels (0..1) of what is playing.
    Levels([f32; crate::meter::BANDS]),
}

/// Bar heights of the five-bar logo.
fn bar_heights(pose: Pose) -> [f32; 5] {
    match pose {
        Pose::Still => [4.0, 14.0, 8.0, 18.0, 6.0],
        Pose::Levels(levels) => levels.map(|level| 3.0 + 17.0 * level),
        Pose::Wave(phase) => {
            let mut out = [0.0; 5];
            for (i, slot) in out.iter_mut().enumerate() {
                let tau = std::f32::consts::TAU;
                *slot = 11.0 + 5.0 * (tau * phase - i as f32 * 0.9).sin() + 2.0 * (2.0 * tau * phase + i as f32).sin();
            }
            out
        }
    }
}

/// Rasterize the logo; `write` stores one pixel given (offset, alpha, r, g, b).
fn rasterize(size: i32, pose: Pose, mut write: impl FnMut(usize, u8, u8, u8, u8)) {
    let scale = size as f32 / 24.0;
    let heights = bar_heights(pose);
    let mut alpha_map = vec![0u8; (size * size) as usize];
    for (i, x) in [4.0f32, 8.0, 12.0, 16.0, 20.0].iter().enumerate() {
        let height = heights[i];
        let (x0, x1) = ((x - 1.25) * scale, (x + 1.25) * scale);
        let (y0, y1) = ((12.0 - height / 2.0) * scale, (12.0 + height / 2.0) * scale);
        let t = i as f32 / 4.0;
        let color = (
            (ACCENT.0 as f32 * (1.0 - t) + PURPLE.0 as f32 * t) as u8,
            (ACCENT.1 as f32 * (1.0 - t) + PURPLE.1 as f32 * t) as u8,
            (ACCENT.2 as f32 * (1.0 - t) + PURPLE.2 as f32 * t) as u8,
        );
        for py in 0..size {
            for px in 0..size {
                let cx = px as f32 + 0.5;
                let cy = py as f32 + 0.5;
                // Soft edges: coverage based on distance to the bar rectangle.
                let dx = (x0 - cx).max(cx - x1).max(0.0);
                let dy = (y0 - cy).max(cy - y1).max(0.0);
                let coverage = (1.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let index = (py * size + px) as usize;
                let alpha = (coverage * 255.0) as u8;
                if alpha > alpha_map[index] {
                    alpha_map[index] = alpha;
                    write(index, alpha, color.0, color.1, color.2);
                }
            }
        }
    }
}

/// ARGB32 in network byte order, as StatusNotifierItem expects.
#[cfg(target_os = "linux")]
pub fn logo_argb(size: i32, pose: Pose) -> Vec<u8> {
    let mut data = vec![0u8; (size * size * 4) as usize];
    rasterize(size, pose, |index, a, r, g, b| {
        data[index * 4..index * 4 + 4].copy_from_slice(&[a, r, g, b]);
    });
    data
}

/// Straight RGBA, as `tray-icon` expects.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn logo_rgba(size: i32, pose: Pose) -> Vec<u8> {
    let mut data = vec![0u8; (size * size * 4) as usize];
    rasterize(size, pose, |index, a, r, g, b| {
        data[index * 4..index * 4 + 4].copy_from_slice(&[r, g, b, a]);
    });
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_has_opaque_bars_and_transparent_margins() {
        let mut data = vec![0u8; 32 * 32 * 4];
        rasterize(32, Pose::Still, |index, a, r, g, b| {
            data[index * 4..index * 4 + 4].copy_from_slice(&[r, g, b, a]);
        });
        let alpha = |x: usize, y: usize| data[(y * 32 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(31, 31), 0);
        // Centre of the tallest (fourth) bar at x = 16/24 of the width.
        assert_eq!(alpha(21, 16), 255);
        assert_ne!(bar_heights(Pose::Wave(0.25)), bar_heights(Pose::Wave(0.75)));
        assert!(bar_heights(Pose::Levels([1.0; 5]))[0] > bar_heights(Pose::Levels([0.0; 5]))[0]);
    }
}
