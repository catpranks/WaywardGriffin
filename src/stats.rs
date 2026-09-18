//! Frame timing aggregation shared by the TUI and the control socket.

use crate::GlobalState;
use crate::sizer::SharedSizer;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Timestamps collected for one captured frame as it moves through the pipeline.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// Capture call started.
    pub start: Instant,
    /// Capture returned a frame (NVFBC grab done, buffer chosen).
    pub wait: Instant,
    /// Frame data is in a Vulkan image (copy + sync done).
    pub obtain: Instant,
    /// Command buffer submitted and surface committed.
    pub commit: Option<Instant>,
    /// Source-side capture timestamp on CLOCK_MONOTONIC.
    pub capture_mono_ns: u64,
    /// Capture timestamp to compositor presentation, from presentation feedback.
    pub present: Option<Duration>,
    pub cursor_visible: bool,
}

impl FrameInfo {
    pub fn mark_commit(&mut self) {
        self.commit = Some(Instant::now());
    }

    pub fn set_present(&mut self, presented_mono_ns: u64) {
        self.present = Some(Duration::from_nanos(
            presented_mono_ns.saturating_sub(self.capture_mono_ns),
        ));
    }

    pub fn wait_ms(&self) -> f64 {
        ms(self.wait.duration_since(self.start))
    }

    pub fn obtain_ms(&self) -> f64 {
        ms(self.obtain.duration_since(self.wait))
    }

    pub fn commit_ms(&self) -> f64 {
        self.commit
            .map(|c| ms(c.duration_since(self.obtain)))
            .unwrap_or(0.0)
    }

    pub fn present_ms(&self) -> f64 {
        self.present.map(ms).unwrap_or(0.0)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Point-in-time summary of the pipeline. Serialized as-is on the control socket.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub uptime_s: f64,
    /// Frames presented per second (over the last 2s).
    pub render_hz: f64,
    /// New frames obtained from the capture backend per second.
    pub capture_hz: f64,
    /// Render callbacks where the backend had no new frame, per second.
    pub skip_hz: f64,
    /// Frames the backend reported as missed, per second.
    pub miss_hz: f64,
    /// Per-frame averages over the timing window, in milliseconds.
    pub wait_ms: f64,
    pub obtain_ms: f64,
    pub commit_ms: f64,
    pub present_ms: f64,
    pub present_min_ms: f64,
    pub present_max_ms: f64,
    /// Number of frames in the timing window.
    pub frames: usize,
    pub confine: bool,
    pub capture: bool,
    pub force_relative: bool,
    pub cursor_visible: bool,
    pub source_size: (u32, u32),
    pub window_size: (u32, u32),
    pub render_size: (u32, u32),
}

const RATE_WINDOW: Duration = Duration::from_secs(2);

pub struct Stats {
    start: Instant,
    timings: VecDeque<FrameInfo>,
    /// How many frames of timing history to keep.
    pub history: usize,
    renders: VecDeque<Instant>,
    captures: VecDeque<Instant>,
    capture_misses: VecDeque<Instant>,
    skips: VecDeque<Instant>,
}

impl Stats {
    pub fn new(history: usize) -> Self {
        Self {
            start: Instant::now(),
            timings: VecDeque::new(),
            history,
            renders: VecDeque::new(),
            captures: VecDeque::new(),
            capture_misses: VecDeque::new(),
            skips: VecDeque::new(),
        }
    }

    pub fn render(&mut self, info: FrameInfo) {
        self.renders.push_back(info.start);
        self.timings.push_back(info);
    }

    pub fn skip(&mut self, t: Instant) {
        self.skips.push_back(t);
    }

    pub fn capture(&mut self, t: Instant) {
        self.captures.push_back(t);
    }

    pub fn capture_miss(&mut self, t: Instant) {
        self.capture_misses.push_back(t);
    }

    /// Drop samples outside the history and rate windows. Call before reading.
    pub fn trim(&mut self) {
        while self.timings.len() > self.history {
            self.timings.pop_front();
        }
        let cutoff = Instant::now() - RATE_WINDOW;
        for q in [
            &mut self.renders,
            &mut self.captures,
            &mut self.capture_misses,
            &mut self.skips,
        ] {
            while q.front().is_some_and(|t| *t < cutoff) {
                q.pop_front();
            }
        }
    }

    /// Present latency per frame, oldest first. Chart input.
    pub fn present_series(&self) -> impl Iterator<Item = f64> + '_ {
        self.timings.iter().map(FrameInfo::present_ms)
    }

    pub fn timings_len(&self) -> usize {
        self.timings.len()
    }

    pub fn snapshot(&self, global_state: &GlobalState, sizer: &SharedSizer) -> Snapshot {
        let n = self.timings.len();
        let (mut wait, mut obtain, mut commit, mut present) = (0.0, 0.0, 0.0, 0.0);
        let (mut present_min, mut present_max) = (f64::MAX, f64::MIN);
        for t in &self.timings {
            wait += t.wait_ms();
            obtain += t.obtain_ms();
            commit += t.commit_ms();
            let p = t.present_ms();
            present += p;
            if t.present.is_some() {
                present_min = present_min.min(p);
                present_max = present_max.max(p);
            }
        }
        let avg = |sum: f64| if n == 0 { 0.0 } else { sum / n as f64 };
        if present_min > present_max {
            present_min = 0.0;
            present_max = 0.0;
        }

        let state = global_state.load();
        let sizer = sizer.load();
        Snapshot {
            uptime_s: self.start.elapsed().as_secs_f64(),
            render_hz: rate(&self.renders),
            capture_hz: rate(&self.captures),
            skip_hz: rate(&self.skips),
            miss_hz: rate(&self.capture_misses),
            wait_ms: avg(wait),
            obtain_ms: avg(obtain),
            commit_ms: avg(commit),
            present_ms: avg(present),
            present_min_ms: present_min,
            present_max_ms: present_max,
            frames: n,
            confine: state.confine,
            capture: state.capture,
            force_relative: state.force_relative,
            cursor_visible: self.timings.back().is_some_and(|t| t.cursor_visible),
            source_size: sizer.source_size,
            window_size: sizer.window_size,
            render_size: sizer.render_size,
        }
    }
}

fn rate(events: &VecDeque<Instant>) -> f64 {
    let (Some(first), Some(last)) = (events.front(), events.back()) else {
        return 0.0;
    };
    if events.len() < 2 {
        return 0.0;
    }
    (events.len() - 1) as f64 / last.duration_since(*first).as_secs_f64().max(1e-9)
}
