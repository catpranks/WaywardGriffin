//! Main-thread event sink: collects log lines and frame timings from the other
//! threads, publishes a stats snapshot, and either drives the TUI or prints logs.

use crate::GlobalState;
use crate::sizer::SharedSizer;
pub use crate::stats::FrameInfo;
use crate::stats::{Snapshot, Stats};
use anyhow::Result;
use arc_swap::ArcSwap;
use crossterm::event::{self, Event, KeyCode};
use ratatui::DefaultTerminal;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug)]
enum PlotEvent {
    Log(String),
    Render(FrameInfo),
    Skip(Instant),
    Capture(Instant),
    CaptureMiss(Instant),
    Fatal(Result<()>),
}

#[derive(Clone)]
pub struct PlotterHandle(mpsc::SyncSender<PlotEvent>);

impl PlotterHandle {
    pub fn log(&self, msg: impl Into<String>) {
        let _ = self.0.try_send(PlotEvent::Log(msg.into()));
    }

    pub fn render(&self, info: FrameInfo) {
        let _ = self.0.try_send(PlotEvent::Render(info));
    }

    pub fn skip(&self) {
        let _ = self.0.try_send(PlotEvent::Skip(Instant::now()));
    }

    pub fn capture(&self) {
        let _ = self.0.try_send(PlotEvent::Capture(Instant::now()));
    }

    pub fn capture_miss(&self) {
        let _ = self.0.try_send(PlotEvent::CaptureMiss(Instant::now()));
    }

    /// Terminates the process with `res`. Every thread entry point wraps its result in this.
    pub fn fatal(&self, res: Result<()>) {
        let _ = self.0.send(PlotEvent::Fatal(res));
    }
}

pub enum Mode {
    /// ratatui dashboard redrawn every `delay`.
    Tui { delay: Duration },
    /// Logs to stdout; optionally a stats line every `stats_every`.
    Headless { stats_every: Option<Duration> },
}

pub struct Plotter {
    handle: PlotterHandle,
    rx: mpsc::Receiver<PlotEvent>,
    global_state: GlobalState,
    sizer: SharedSizer,
    published: Arc<ArcSwap<Snapshot>>,
}

/// Frames of timing history per terminal column in the chart.
const DENSITY_FACTOR: usize = 8;
const HEADLESS_HISTORY: usize = 600;
const PUBLISH_EVERY: Duration = Duration::from_millis(100);

impl Plotter {
    pub fn new(global_state: GlobalState, sizer: SharedSizer) -> Self {
        let (tx, rx) = mpsc::sync_channel(1024);
        Self {
            handle: PlotterHandle(tx),
            rx,
            global_state,
            sizer,
            published: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
        }
    }

    pub fn handle(&self) -> PlotterHandle {
        self.handle.clone()
    }

    /// Latest stats snapshot, refreshed every 100ms while `run` is active.
    pub fn snapshot(&self) -> Arc<ArcSwap<Snapshot>> {
        self.published.clone()
    }

    pub fn run(self, mode: Mode) -> Result<()> {
        let res = match mode {
            Mode::Tui { delay } => {
                let terminal = ratatui::init();
                let res = self.run_tui(delay, terminal);
                ratatui::restore();
                res
            }
            Mode::Headless { stats_every } => self.run_headless(stats_every),
        };
        // Give other threads a moment to report, then show anything they said.
        std::thread::sleep(Duration::from_secs(1));
        let mut printed = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                PlotEvent::Log(msg) => {
                    if !printed {
                        printed = true;
                        eprintln!("late log output:");
                    }
                    eprint!("{msg}");
                }
                PlotEvent::Fatal(Err(e)) => eprintln!("late thread failure: {e:?}"),
                _ => {}
            }
        }
        res
    }

    fn publish(&self, stats: &mut Stats) {
        stats.trim();
        self.published
            .store(Arc::new(stats.snapshot(&self.global_state, &self.sizer)));
    }

    fn run_headless(&self, stats_every: Option<Duration>) -> Result<()> {
        let mut stats = Stats::new(HEADLESS_HISTORY);
        let mut last_publish = Instant::now();
        let mut last_stats_log = Instant::now();
        loop {
            match self.rx.recv_timeout(PUBLISH_EVERY) {
                Ok(PlotEvent::Log(msg)) => print!("{msg}"),
                Ok(PlotEvent::Render(info)) => stats.render(info),
                Ok(PlotEvent::Skip(t)) => stats.skip(t),
                Ok(PlotEvent::Capture(t)) => stats.capture(t),
                Ok(PlotEvent::CaptureMiss(t)) => stats.capture_miss(t),
                Ok(PlotEvent::Fatal(res)) => return res,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if last_publish.elapsed() >= PUBLISH_EVERY {
                self.publish(&mut stats);
                last_publish = Instant::now();
            }
            if let Some(every) = stats_every
                && last_stats_log.elapsed() >= every
            {
                last_stats_log = Instant::now();
                let snap = self.published.load();
                info!(target: "stats", "{}", serde_json::to_string(&**snap)?);
            }
        }
    }

    fn run_tui(&self, delay: Duration, mut terminal: DefaultTerminal) -> Result<()> {
        let mut app = TuiApp {
            logs: Vec::new(),
            log_scroll: 0,
            stats: Stats::new(1),
        };
        loop {
            app.stats.history = terminal.size()?.width.saturating_sub(5) as usize * DENSITY_FACTOR;
            self.publish(&mut app.stats);
            let snap = self.published.load();
            terminal.draw(|f| app.ui(f, &snap))?;

            if event::poll(delay)?
                && let Event::Key(key) = event::read()?
            {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::PageUp => {
                        let size = terminal.size()?;
                        let wrapped = app
                            .logs
                            .iter()
                            .map(|m| m.len().div_ceil(size.width as usize))
                            .sum::<usize>();
                        let log_height = (size.height as usize).saturating_sub(4);
                        let max_scroll = wrapped.saturating_sub(log_height);
                        app.log_scroll = (app.log_scroll + 10).min(max_scroll);
                    }
                    KeyCode::PageDown => {
                        app.log_scroll = app.log_scroll.saturating_sub(10);
                    }
                    _ => {}
                }
            }

            for event in self.rx.try_iter() {
                match event {
                    PlotEvent::Log(msg) => {
                        let msg = msg.trim_end_matches('\n');
                        app.logs.extend(msg.split('\n').map(str::to_owned));
                        app.log_scroll = 0;
                    }
                    PlotEvent::Render(info) => app.stats.render(info),
                    PlotEvent::Skip(t) => app.stats.skip(t),
                    PlotEvent::Capture(t) => app.stats.capture(t),
                    PlotEvent::CaptureMiss(t) => app.stats.capture_miss(t),
                    PlotEvent::Fatal(res) => return res,
                }
            }
        }
    }
}

struct TuiApp {
    logs: Vec<String>,
    log_scroll: usize,
    stats: Stats,
}

impl TuiApp {
    fn ui(&self, f: &mut Frame, snap: &Snapshot) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Percentage(50), // Chart
                Constraint::Length(1),      // Status
                Constraint::Min(7),         // Log
            ])
            .split(f.area());

        let n = self.stats.timings_len();
        let present_data: Vec<(f64, f64)> = self
            .stats
            .present_series()
            .enumerate()
            .map(|(i, p)| (i as f64 - (n as f64 - 1.0), p))
            .collect();
        let x_axis_bounds = [-(self.stats.history as f64 - 1.0), 0.0];
        let max_timing = present_data.iter().map(|(_, p)| *p).fold(0.0, f64::max);
        let y_axis_bounds = [0.0, (max_timing * 1.2).max(16.67)];
        let y_labels: Vec<Line> = vec![
            "0.0".into(),
            format!("{:.1}", y_axis_bounds[1] / 2.0).into(),
            format!("{:.1}", y_axis_bounds[1]).into(),
        ];
        let datasets = vec![
            Dataset::default()
                .name("Present")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Yellow))
                .data(&present_data),
        ];
        let chart_title = format!(
            "Present latency {}x{} -> {}x{}",
            snap.source_size.0, snap.source_size.1, snap.render_size.0, snap.render_size.1
        );
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(chart_title))
            .x_axis(
                Axis::default()
                    .style(Style::default().fg(Color::Gray))
                    .bounds(x_axis_bounds),
            )
            .y_axis(
                Axis::default()
                    .title("ms")
                    .style(Style::default().fg(Color::Gray))
                    .bounds(y_axis_bounds)
                    .labels(y_labels),
            );
        f.render_widget(chart, chunks[0]);

        let mut tags = Vec::new();
        if snap.confine {
            tags.push("G");
        }
        if snap.capture {
            tags.push("C");
        }
        if snap.force_relative {
            tags.push("FR");
        }
        if snap.cursor_visible {
            tags.push("Cur");
        }
        let status_text = format!(
            "R{:.1} C{:.1} | S:{:.1} M:{:.1} [{}] | W:{:.2} O:{:.2} C:{:.2} P:{:.2} ({:.2}-{:.2})",
            snap.render_hz,
            snap.capture_hz,
            snap.skip_hz,
            snap.miss_hz,
            tags.join(" "),
            snap.wait_ms,
            snap.obtain_ms,
            snap.commit_ms,
            snap.present_ms,
            snap.present_min_ms,
            snap.present_max_ms,
        );
        f.render_widget(Paragraph::new(status_text), chunks[1]);

        let log_area = chunks[2];
        let width = log_area.width as usize;
        let log_height = log_area.height as usize;
        let wrapped_logs: Vec<String> = self
            .logs
            .iter()
            .flat_map(|m| {
                if m.len() <= width {
                    vec![m.clone()]
                } else {
                    m.chars()
                        .collect::<Vec<_>>()
                        .chunks(width.max(1))
                        .map(|chunk| chunk.iter().collect::<String>())
                        .collect()
                }
            })
            .collect();
        let start_idx = if wrapped_logs.len() > log_height && self.log_scroll < wrapped_logs.len() {
            wrapped_logs
                .len()
                .saturating_sub(log_height + self.log_scroll)
        } else {
            0
        };
        let log_lines: Vec<ListItem> = wrapped_logs
            .iter()
            .skip(start_idx)
            .take(log_height)
            .map(|m| ListItem::new(Line::from(m.as_str())))
            .collect();
        f.render_widget(List::new(log_lines), log_area);
    }
}
