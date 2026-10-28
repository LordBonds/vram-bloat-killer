//! Decides when a process's GPU memory is bloat rather than normal use.
//!
//! Games and 3D apps are supposed to use lots of VRAM, so size alone only counts
//! against browsers. For everything else the signs are borrowed system RAM and
//! steady growth that never drops back.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::gpu::GpuProcess;

/// How often a process's usage is written into its history.
pub const CHECKPOINT: Duration = Duration::from_secs(5 * 60);
/// Seven checkpoints five minutes apart: the last 30 minutes.
pub const WINDOW: usize = 7;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reason {
    /// Total GPU memory is over the auto-kill limit.
    OverKill,
    /// A browser/Electron GPU process is over the warn limit.
    OverWarn,
    /// Another app borrowed more system RAM than the warn limit.
    Borrowed,
    /// Climbed this many bytes over 30 minutes without dropping back.
    Growing(u64),
}

pub struct Limits {
    pub warn: u64,
    pub growth: u64,
    pub kill: Option<u64>,
}

#[derive(Default)]
pub struct Watch {
    tracks: HashMap<u32, Track>,
}

struct Track {
    points: VecDeque<u64>,
    next: Instant,
}

impl Watch {
    /// Records history and returns every process that looks bloated, with why.
    pub fn assess(&mut self, procs: &[GpuProcess], limits: &Limits) -> Vec<(u32, Reason)> {
        let now = Instant::now();
        self.tracks.retain(|pid, _| procs.iter().any(|p| p.pid == *pid));
        procs
            .iter()
            .filter_map(|p| {
                let t = self.tracks.entry(p.pid).or_insert_with(|| Track { points: VecDeque::new(), next: now });
                if now >= t.next {
                    t.points.push_back(p.total());
                    if t.points.len() > WINDOW {
                        t.points.pop_front();
                    }
                    t.next = now + CHECKPOINT;
                }
                judge(p, t.points.make_contiguous(), limits).map(|r| (p.pid, r))
            })
            .collect()
    }
}

pub fn judge(p: &GpuProcess, history: &[u64], l: &Limits) -> Option<Reason> {
    if l.kill.is_some_and(|k| p.total() >= k) {
        return Some(Reason::OverKill);
    }
    if p.restartable && p.total() >= l.warn {
        return Some(Reason::OverWarn);
    }
    // With integrated graphics all GPU memory is shared, so borrowed RAM only
    // means something for a process that also has a dedicated card.
    if !p.restartable && p.dedicated > 0 && p.shared >= l.warn {
        return Some(Reason::Borrowed);
    }
    leak_rise(history, l.growth).map(Reason::Growing)
}

/// A leak climbs in most intervals and never really drops. A level load is one
/// big step, and normal use goes up and down, so neither counts.
pub fn leak_rise(points: &[u64], growth: u64) -> Option<u64> {
    if points.len() < WINDOW {
        return None;
    }
    let rise = points[points.len() - 1].saturating_sub(points[0]);
    let dropped = points.windows(2).any(|w| w[1] < w[0] - w[0] / 20);
    let climbs = points.windows(2).filter(|w| w[1] > w[0] + w[0] / 100).count();
    (rise >= growth && !dropped && climbs >= 4).then_some(rise)
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn proc(restartable: bool, shared: u64, dedicated: u64) -> GpuProcess {
        GpuProcess { pid: 1, app: "x".into(), shared, dedicated, restartable, owner: 1, protected: false }
    }

    fn limits() -> Limits {
        Limits { warn: 3 * G, growth: 2 * G, kill: Some(6 * G) }
    }

    #[test]
    fn steady_climb_is_a_leak() {
        let pts: Vec<u64> = (0..7).map(|i| G + i * G / 2).collect();
        assert_eq!(leak_rise(&pts, 2 * G), Some(3 * G));
    }

    #[test]
    fn one_big_step_is_a_load_not_a_leak() {
        let pts = [G, G, G, 4 * G, 4 * G, 4 * G, 4 * G];
        assert_eq!(leak_rise(&pts, 2 * G), None);
    }

    #[test]
    fn a_drop_means_it_frees_memory() {
        let pts = [G, 2 * G, 3 * G, 2 * G, 3 * G, 4 * G, 5 * G];
        assert_eq!(leak_rise(&pts, 2 * G), None);
    }

    #[test]
    fn needs_a_full_window() {
        let pts = [G, 2 * G, 3 * G, 4 * G];
        assert_eq!(leak_rise(&pts, 2 * G), None);
    }

    #[test]
    fn small_climb_is_under_the_threshold() {
        let pts: Vec<u64> = (0..7).map(|i| G + i * G / 10).collect();
        assert_eq!(leak_rise(&pts, 2 * G), None);
    }

    #[test]
    fn a_big_game_is_not_bloat() {
        assert_eq!(judge(&proc(false, G / 4, 5 * G), &[], &limits()), None);
    }

    #[test]
    fn a_big_browser_is() {
        assert_eq!(judge(&proc(true, 3 * G, G / 2), &[], &limits()), Some(Reason::OverWarn));
    }

    #[test]
    fn borrowed_ram_counts_only_beside_a_dedicated_card() {
        assert_eq!(judge(&proc(false, 4 * G, G), &[], &limits()), Some(Reason::Borrowed));
        assert_eq!(judge(&proc(false, 4 * G, 0), &[], &limits()), None);
    }

    #[test]
    fn kill_limit_applies_to_everything_and_wins() {
        assert_eq!(judge(&proc(false, 0, 7 * G), &[], &limits()), Some(Reason::OverKill));
        assert_eq!(judge(&proc(true, 5 * G, 2 * G), &[], &limits()), Some(Reason::OverKill));
        let off = Limits { kill: None, ..limits() };
        assert_eq!(judge(&proc(false, 0, 7 * G), &[], &off), None);
    }
}
