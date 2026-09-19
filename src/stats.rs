//! Frame timing aggregation shared by the TUI and the control socket.

use crate::GlobalState;
use crate::sizer::SharedSizer;
use crate::utils::clock_monotonic_ns;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Timestamps collected for one captured frame as it moves through the pipeline.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// Capture call started.
    pub start: Instant,
    /// Capture returned a frame (NVFBC grab done, before choosing a buffer).
    pub wait: Instant,
    /// Frame data is in a Vulkan image (copy + sync done).
    pub obtain: Instant,
    /// CPU timestamp just before queue submission (not a Wayland commit).
    pub commit: Option<Instant>,
    /// Source-side capture timestamp on CLOCK_MONOTONIC.
    pub capture_mono_ns: u64,
    /// Capture timestamp to compositor presentation, from presentation feedback.
    pub present: Option<Duration>,
    pub cursor_visible: bool,
    pub latency: FrameLatency,
}

/// CPU and presentation-clock measurements; absent source fields on Wayland backends.
#[derive(Debug, Clone, Default)]
pub struct FrameLatency {
    pub grab_mono_ns: Option<u64>,
    pub submit_mono_ns: Option<u64>,
    pub present_return_mono_ns: Option<u64>,
    pub presented_mono_ns: Option<u64>,
    pub feedback_mono_ns: Option<u64>,
    pub render_queue: Duration,
    pub fence_wait: Duration,
    pub acquire: Duration,
    pub queue_submit: Duration,
    pub queue_present: Option<Duration>,
    pub direct_capture: Option<bool>,
    pub presentation_flags: Option<u32>,
    /// Output refresh period reported with the presentation feedback.
    pub refresh_ns: Option<u32>,
}

impl FrameInfo {
    pub fn mark_commit(&mut self) {
        self.commit = Some(Instant::now());
        self.latency.submit_mono_ns = Some(clock_monotonic_ns());
    }

    pub fn set_present(&mut self, presented_mono_ns: u64) {
        self.latency.presented_mono_ns = Some(presented_mono_ns);
        self.present = presented_mono_ns
            .checked_sub(self.capture_mono_ns)
            .map(Duration::from_nanos);
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

    /// Submission start → compositor presentation, when both clocks are known.
    pub fn submit_to_present_ms(&self) -> Option<f64> {
        ns_between(
            self.latency.presented_mono_ns?,
            self.latency.submit_mono_ns?,
        )
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
    /// Captured images replaced or dropped before submission.
    pub capture_drop_hz: f64,
    /// Per-frame averages over the timing window, in milliseconds.
    pub wait_ms: f64,
    pub obtain_ms: f64,
    pub commit_ms: f64,
    pub present_ms: f64,
    pub present_min_ms: f64,
    pub present_max_ms: f64,
    /// Number of frames in the timing window.
    pub frames: usize,
    /// Valid source-to-presentation samples (clock mismatch is not zero latency).
    pub present_frames: usize,
    pub age_at_grab_ms: Option<f64>,
    pub grab_to_submit_ms: Option<f64>,
    pub render_queue_ms: f64,
    pub fence_wait_ms: f64,
    pub acquire_ms: f64,
    pub queue_submit_ms: f64,
    pub queue_present_ms: Option<f64>,
    pub queue_present_max_ms: Option<f64>,
    /// Includes GPU execution, WSI and compositor scheduling; overlaps queue_present_ms.
    pub submit_to_present_ms: Option<f64>,
    /// Signed: the presentation can occur before queue_present returns.
    pub present_return_to_present_ms: Option<f64>,
    pub feedback_delay_ms: Option<f64>,
    pub feedback_age_ms: Option<f64>,
    pub direct_capture_fraction: Option<f64>,
    pub zero_copy_fraction: Option<f64>,
    /// Raw wp_presentation_feedback flags from the latest displayed frame.
    pub presentation_flags: Option<u32>,
    /// Output refresh period from the latest presentation feedback.
    pub refresh_ms: Option<f64>,
    pub frame_callback_hz: f64,
    pub frame_callback_interval_ms: Option<f64>,
    pub render_idle_ms: Option<f64>,
    pub submit_hz: f64,
    pub discard_hz: f64,
    pub feedback_timeout_hz: f64,
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
    capture_drops: VecDeque<Instant>,
    skips: VecDeque<Instant>,
    callbacks: VecDeque<Instant>,
    idles: VecDeque<(Instant, Duration)>,
    submits: VecDeque<Instant>,
    discards: VecDeque<Instant>,
    feedback_timeouts: VecDeque<Instant>,
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
            capture_drops: VecDeque::new(),
            skips: VecDeque::new(),
            callbacks: VecDeque::new(),
            idles: VecDeque::new(),
            submits: VecDeque::new(),
            discards: VecDeque::new(),
            feedback_timeouts: VecDeque::new(),
        }
    }

    pub fn render(&mut self, info: FrameInfo) {
        self.renders.push_back(Instant::now());
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

    pub fn capture_drop(&mut self, t: Instant) {
        self.capture_drops.push_back(t);
    }

    pub fn callback(&mut self, t: Instant) {
        self.callbacks.push_back(t);
    }
    pub fn idle(&mut self, t: Instant, duration: Duration) {
        self.idles.push_back((t, duration));
    }
    pub fn submit(&mut self, t: Instant) {
        self.submits.push_back(t);
    }
    pub fn discard(&mut self, t: Instant) {
        self.discards.push_back(t);
    }
    pub fn feedback_timeout(&mut self, t: Instant) {
        self.feedback_timeouts.push_back(t);
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
            &mut self.capture_drops,
            &mut self.skips,
            &mut self.callbacks,
            &mut self.submits,
            &mut self.discards,
            &mut self.feedback_timeouts,
        ] {
            while q.front().is_some_and(|t| *t < cutoff) {
                q.pop_front();
            }
        }
        while self.idles.front().is_some_and(|(t, _)| *t < cutoff) {
            self.idles.pop_front();
        }
    }

    /// Present latency per frame, oldest first; `None` without valid feedback. Chart input.
    pub fn present_series(&self) -> impl Iterator<Item = Option<f64>> + '_ {
        self.timings.iter().map(|t| t.present.map(ms))
    }

    /// Submit→present per frame, oldest first. Chart input.
    pub fn submit_to_present_series(&self) -> impl Iterator<Item = Option<f64>> + '_ {
        self.timings.iter().map(FrameInfo::submit_to_present_ms)
    }

    pub fn timings_len(&self) -> usize {
        self.timings.len()
    }

    /// Time between the oldest and newest frame in the timing window.
    pub fn span(&self) -> Duration {
        match (self.timings.front(), self.timings.back()) {
            (Some(first), Some(last)) => last.start.duration_since(first.start),
            _ => Duration::ZERO,
        }
    }

    pub fn snapshot(&self, global_state: &GlobalState, sizer: &SharedSizer) -> Snapshot {
        let n = self.timings.len();
        let present_frames = self.timings.iter().filter(|t| t.present.is_some()).count();
        let mean = |f: fn(&FrameInfo) -> Option<f64>| average(self.timings.iter().filter_map(f));
        let rate = |events: &VecDeque<Instant>| rate(events, self.start.elapsed().min(RATE_WINDOW));
        let callback_hz = rate(&self.callbacks);
        let callback_interval = self
            .callbacks
            .front()
            .zip(self.callbacks.back())
            .filter(|_| self.callbacks.len() > 1)
            .map(|(first, last)| {
                ms(last.duration_since(*first)) / (self.callbacks.len() - 1) as f64
            });
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
            capture_drop_hz: rate(&self.capture_drops),
            wait_ms: avg(wait),
            obtain_ms: avg(obtain),
            commit_ms: avg(commit),
            present_ms: if present_frames == 0 {
                0.0
            } else {
                present / present_frames as f64
            },
            present_min_ms: present_min,
            present_max_ms: present_max,
            frames: n,
            present_frames,
            age_at_grab_ms: mean(|t| ns_between(t.latency.grab_mono_ns?, t.capture_mono_ns)),
            grab_to_submit_ms: mean(|t| {
                ns_between(t.latency.submit_mono_ns?, t.latency.grab_mono_ns?)
            }),
            render_queue_ms: mean(|t| Some(ms(t.latency.render_queue))).unwrap_or(0.0),
            fence_wait_ms: mean(|t| Some(ms(t.latency.fence_wait))).unwrap_or(0.0),
            acquire_ms: mean(|t| Some(ms(t.latency.acquire))).unwrap_or(0.0),
            queue_submit_ms: mean(|t| Some(ms(t.latency.queue_submit))).unwrap_or(0.0),
            queue_present_ms: mean(|t| t.latency.queue_present.map(ms)),
            queue_present_max_ms: self
                .timings
                .iter()
                .filter_map(|t| t.latency.queue_present.map(ms))
                .reduce(f64::max),
            submit_to_present_ms: mean(FrameInfo::submit_to_present_ms),
            present_return_to_present_ms: mean(|t| {
                Some(
                    (t.latency.presented_mono_ns? as i128
                        - t.latency.present_return_mono_ns? as i128) as f64
                        / 1e6,
                )
            }),
            feedback_delay_ms: mean(|t| {
                ns_between(t.latency.feedback_mono_ns?, t.latency.presented_mono_ns?)
            }),
            feedback_age_ms: self
                .timings
                .back()
                .and_then(|t| ns_between(clock_monotonic_ns(), t.latency.feedback_mono_ns?)),
            direct_capture_fraction: mean(|t| Some(u8::from(t.latency.direct_capture?) as f64)),
            zero_copy_fraction: mean(|t| {
                Some(u8::from(t.latency.presentation_flags? & 8 != 0) as f64)
            }),
            presentation_flags: self
                .timings
                .back()
                .and_then(|t| t.latency.presentation_flags),
            refresh_ms: self
                .timings
                .iter()
                .rev()
                .find_map(|t| t.latency.refresh_ns)
                .map(|ns| f64::from(ns) / 1e6),
            frame_callback_hz: callback_hz,
            frame_callback_interval_ms: callback_interval,
            render_idle_ms: average(self.idles.iter().map(|(_, d)| ms(*d))),
            submit_hz: rate(&self.submits),
            discard_hz: rate(&self.discards),
            feedback_timeout_hz: rate(&self.feedback_timeouts),
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

// Counts over the whole observation window also handle sparse/batched misses.
// Dividing by first-to-last event time reports millions of Hz for one missed batch.
fn rate(events: &VecDeque<Instant>, window: Duration) -> f64 {
    events.len() as f64 / window.as_secs_f64().max(1e-9)
}

fn ns_between(end: u64, start: u64) -> Option<f64> {
    end.checked_sub(start).map(|ns| ns as f64 / 1e6)
}

fn average(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0), |(sum, n), v| (sum + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    fn snapshot(stats: &Stats) -> Snapshot {
        stats.snapshot(
            &Arc::new(ArcSwap::from_pointee(Default::default())),
            &Arc::new(ArcSwap::from_pointee(Default::default())),
        )
    }

    fn frame() -> FrameInfo {
        let start = Instant::now();
        FrameInfo {
            start,
            wait: start,
            obtain: start,
            commit: None,
            capture_mono_ns: 1_000_000,
            present: None,
            cursor_visible: false,
            latency: FrameLatency {
                grab_mono_ns: Some(3_000_000),
                submit_mono_ns: Some(4_000_000),
                present_return_mono_ns: Some(12_000_000),
                feedback_mono_ns: Some(13_000_000),
                queue_present: Some(Duration::from_millis(8)),
                direct_capture: Some(true),
                presentation_flags: Some(8),
                ..Default::default()
            },
        }
    }

    #[test]
    fn sparse_batched_events_use_the_whole_rate_window() {
        let now = Instant::now();
        let events = VecDeque::from([now, now]);
        assert_eq!(rate(&events, RATE_WINDOW), 1.0);
    }

    #[test]
    fn presentation_breakdown_excludes_feedback_delivery_and_handles_late_return() {
        let mut stats = Stats::new(10);
        let mut frame = frame();
        frame.set_present(11_000_000);
        stats.render(frame);
        let s = snapshot(&stats);
        assert_eq!(s.present_ms, 10.0);
        assert_eq!(s.age_at_grab_ms, Some(2.0));
        assert_eq!(s.grab_to_submit_ms, Some(1.0));
        assert_eq!(s.submit_to_present_ms, Some(7.0));
        assert_eq!(s.feedback_delay_ms, Some(2.0));
        assert_eq!(s.present_return_to_present_ms, Some(-1.0));
        assert_eq!(s.zero_copy_fraction, Some(1.0));
        assert_eq!(s.direct_capture_fraction, Some(1.0));
    }

    #[test]
    fn missing_or_invalid_timestamps_do_not_dilute_latency() {
        let mut stats = Stats::new(10);
        assert_eq!(snapshot(&stats).age_at_grab_ms, None);
        let mut valid = frame();
        valid.set_present(11_000_000);
        stats.render(valid);
        stats.render(frame()); // Unsupported presentation clock: no timestamp.
        let mut invalid = frame();
        invalid.set_present(0); // Presentation precedes the source timestamp.
        stats.render(invalid);
        let s = snapshot(&stats);
        assert_eq!(s.present_frames, 1);
        assert_eq!(s.present_ms, 10.0);
        assert_eq!(s.submit_to_present_ms, Some(7.0));
    }
}
