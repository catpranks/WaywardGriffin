//! Main-thread event sink: collects log lines and frame timings from the other
//! threads, publishes a stats snapshot, and either drives the TUI or prints logs.

use crate::GlobalState;
use crate::sizer::SharedSizer;
pub use crate::stats::FrameInfo;
use crate::stats::{Snapshot, Stats};
use anyhow::Result;
use arc_swap::ArcSwap;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use tracing::{Level, info};
use tracing_subscriber::layer::Context;

/// One tracing event, kept structured so the TUI can colour and align it.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: Instant,
    pub wall: SystemTime,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogLine {
    /// Headless / late-output format: wall time, level, target, message.
    fn headless(&self) -> String {
        format!(
            "{} {:>5} {}: {}",
            humantime::format_rfc3339_millis(self.wall),
            self.level,
            self.target,
            self.message
        )
    }
}

#[derive(Debug)]
enum MonitorEvent {
    Log(LogLine),
    Render(Box<FrameInfo>),
    Skip(Instant),
    Capture(Instant),
    CaptureMiss(Instant),
    CaptureDrop(Instant),
    Callback(Instant),
    Idle(Instant, Duration),
    Submit(Instant),
    Discard(Instant),
    FeedbackTimeout(Instant),
    Fatal(Result<()>),
}

#[derive(Clone)]
pub struct MonitorHandle(mpsc::SyncSender<MonitorEvent>);

impl MonitorHandle {
    fn log(&self, line: LogLine) {
        let _ = self.0.try_send(MonitorEvent::Log(line));
    }

    pub fn render(&self, info: FrameInfo) {
        let _ = self.0.try_send(MonitorEvent::Render(Box::new(info)));
    }

    pub fn skip(&self) {
        let _ = self.0.try_send(MonitorEvent::Skip(Instant::now()));
    }

    pub fn capture(&self) {
        let _ = self.0.try_send(MonitorEvent::Capture(Instant::now()));
    }

    pub fn capture_miss(&self) {
        let _ = self.0.try_send(MonitorEvent::CaptureMiss(Instant::now()));
    }

    pub fn capture_drop(&self) {
        let _ = self.0.try_send(MonitorEvent::CaptureDrop(Instant::now()));
    }

    pub fn callback(&self) {
        let _ = self.0.try_send(MonitorEvent::Callback(Instant::now()));
    }
    pub fn idle(&self, duration: Duration) {
        let _ = self
            .0
            .try_send(MonitorEvent::Idle(Instant::now(), duration));
    }
    pub fn submit(&self) {
        let _ = self.0.try_send(MonitorEvent::Submit(Instant::now()));
    }
    pub fn discard(&self) {
        let _ = self.0.try_send(MonitorEvent::Discard(Instant::now()));
    }
    pub fn feedback_timeout(&self) {
        let _ = self
            .0
            .try_send(MonitorEvent::FeedbackTimeout(Instant::now()));
    }

    /// Terminates the process with `res`. Every thread entry point wraps its result in this.
    pub fn fatal(&self, res: Result<()>) {
        let _ = self.0.send(MonitorEvent::Fatal(res));
    }
}

/// `tracing_subscriber` layer that forwards events to the monitor as `LogLine`s.
pub struct MonitorLayer(pub MonitorHandle);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MonitorLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = FieldVisitor::default();
        event.record(&mut fields);
        let mut message = fields.message;
        message.push_str(&fields.rest);
        self.0.log(LogLine {
            at: Instant::now(),
            wall: SystemTime::now(),
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            message,
        });
    }
}

/// `message` first, then ` key=value` for the other fields, like the compact formatter.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    rest: String,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.rest, " {}={}", field.name(), value);
        }
    }
}

pub enum Mode {
    /// ratatui dashboard redrawn every `delay`.
    Tui { delay: Duration },
    /// Logs to stdout; optionally a stats line every `stats_every`.
    Headless { stats_every: Option<Duration> },
}

pub struct Monitor {
    handle: MonitorHandle,
    rx: mpsc::Receiver<MonitorEvent>,
    global_state: GlobalState,
    sizer: SharedSizer,
    published: Arc<ArcSwap<Snapshot>>,
}

/// Frames of timing history per terminal column in the chart.
const DENSITY_FACTOR: usize = 8;
const HEADLESS_HISTORY: usize = 600;
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Log entries retained by the TUI.
const LOG_CAP: usize = 5000;

impl Monitor {
    pub fn new(global_state: GlobalState, sizer: SharedSizer) -> Self {
        let (tx, rx) = mpsc::sync_channel(1024);
        Self {
            handle: MonitorHandle(tx),
            rx,
            global_state,
            sizer,
            published: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
        }
    }

    pub fn handle(&self) -> MonitorHandle {
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
                MonitorEvent::Log(line) => {
                    if !printed {
                        printed = true;
                        eprintln!("late log output:");
                    }
                    eprintln!("{}", line.headless());
                }
                MonitorEvent::Fatal(Err(e)) => eprintln!("late thread failure: {e:?}"),
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
                Ok(MonitorEvent::Log(line)) => println!("{}", line.headless()),
                Ok(MonitorEvent::Fatal(res)) => return res,
                Ok(event) => stats.event(event),
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

    /// Input and the published snapshot run at 100 ms regardless of `delay`;
    /// `delay` only paces redraws.
    fn run_tui(&self, delay: Duration, mut terminal: DefaultTerminal) -> Result<()> {
        let mut app = TuiApp {
            logs: VecDeque::new(),
            scroll: None,
            stats: Stats::new(1),
            start: Instant::now(),
        };
        let mut last_draw: Option<Instant> = None;
        loop {
            let size = terminal.size()?;
            app.stats.history = (size.width as usize).saturating_sub(CHART_GUTTER) * DENSITY_FACTOR;
            self.publish(&mut app.stats);
            if last_draw.is_none_or(|t| t.elapsed() >= delay) {
                let snap = self.published.load();
                terminal.draw(|f| app.ui(f, &snap))?;
                last_draw = Some(Instant::now());
            }

            if event::poll(PUBLISH_EVERY)? {
                match event::read()? {
                    Event::Key(key) => {
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_c || key.code == KeyCode::Char('q') {
                            return Ok(());
                        }
                        app.scroll_key(key.code, size);
                        last_draw = None;
                    }
                    Event::Resize(..) => last_draw = None,
                    _ => {}
                }
            }

            for event in self.rx.try_iter() {
                match event {
                    MonitorEvent::Log(line) => app.push_log(line),
                    MonitorEvent::Fatal(res) => return res,
                    event => app.stats.event(event),
                }
            }
        }
    }
}

impl Stats {
    fn event(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::Render(info) => self.render(*info),
            MonitorEvent::Skip(t) => self.skip(t),
            MonitorEvent::Capture(t) => self.capture(t),
            MonitorEvent::CaptureMiss(t) => self.capture_miss(t),
            MonitorEvent::CaptureDrop(t) => self.capture_drop(t),
            MonitorEvent::Callback(t) => self.callback(t),
            MonitorEvent::Idle(t, d) => self.idle(t, d),
            MonitorEvent::Submit(t) => self.submit(t),
            MonitorEvent::Discard(t) => self.discard(t),
            MonitorEvent::FeedbackTimeout(t) => self.feedback_timeout(t),
            MonitorEvent::Log(_) | MonitorEvent::Fatal(_) => {}
        }
    }
}

/// Border, y-axis labels and axis line to the left of the plot area.
const CHART_GUTTER: usize = 8;
/// Fallback output refresh period until presentation feedback reports one.
const DEFAULT_REFRESH_MS: f64 = 1000.0 / 60.0;
/// Rows of the status block between chart and log.
const STATUS_ROWS: u16 = 3;

struct LogEntry {
    line: LogLine,
    /// Consecutive identical (level, target, message) lines collapse into one.
    repeats: u32,
}

struct TuiApp {
    logs: VecDeque<LogEntry>,
    /// `None` follows the newest lines; `Some(row)` pins the first visible wrapped row.
    scroll: Option<usize>,
    stats: Stats,
    start: Instant,
}

impl TuiApp {
    fn push_log(&mut self, line: LogLine) {
        if let Some(last) = self.logs.back_mut()
            && last.line.level == line.level
            && last.line.target == line.target
            && last.line.message == line.message
        {
            last.repeats += 1;
            last.line.at = line.at;
            return;
        }
        self.logs.push_back(LogEntry { line, repeats: 1 });
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
            // Approximate: an entry usually occupies one row.
            self.scroll = self.scroll.map(|row| row.saturating_sub(1));
        }
    }

    fn scroll_key(&mut self, code: KeyCode, size: Size) {
        let width = size.width;
        let height = log_area_height(size);
        let total = self.log_paragraph().line_count(width);
        let bottom = total.saturating_sub(height);
        let page = height.saturating_sub(1).max(1);
        let current = self.scroll.unwrap_or(bottom);
        let target = match code {
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => current + page,
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => current + 1,
            KeyCode::Home => 0,
            KeyCode::End => bottom,
            _ => return,
        };
        self.scroll = (target < bottom).then_some(target);
    }

    fn ui(&self, f: &mut Frame, snap: &Snapshot) {
        let area = f.area();
        let chunks = Layout::vertical([
            Constraint::Length(chart_height(area.height)),
            Constraint::Length(STATUS_ROWS),
            Constraint::Min(3),
        ])
        .split(area);
        self.chart(f, chunks[0], snap);
        self.status(f, chunks[1], snap);
        self.log(f, chunks[2]);
    }

    fn chart(&self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let n = self.stats.timings_len();
        let x_of = |i: usize| i as f64 - (n as f64 - 1.0);
        let present: Vec<(f64, f64)> = self
            .stats
            .present_series()
            .enumerate()
            .filter_map(|(i, p)| Some((x_of(i), p?)))
            .collect();
        let submit: Vec<(f64, f64)> = self
            .stats
            .submit_to_present_series()
            .enumerate()
            .filter_map(|(i, p)| Some((x_of(i), p?)))
            .collect();

        // Axis in whole refresh periods: at least two, enough for p99 with headroom.
        let refresh = snap.refresh_ms.unwrap_or(DEFAULT_REFRESH_MS);
        let mut sorted: Vec<f64> = present.iter().map(|(_, p)| *p).collect();
        sorted.sort_by(f64::total_cmp);
        let p99 = sorted
            .get(sorted.len().saturating_sub(1) * 99 / 100)
            .copied()
            .unwrap_or(0.0);
        let periods = ((p99 * 1.2 / refresh).ceil() as usize).max(2);
        let label_step = periods.div_ceil(4);
        let periods = periods.next_multiple_of(label_step);
        let y_max = periods as f64 * refresh;
        let x_bounds = [-(self.stats.history as f64 - 1.0), 0.0];

        let (inside, clipped): (Vec<_>, Vec<_>) = present.iter().partition(|(_, p)| *p <= y_max);
        let clipped: Vec<(f64, f64)> = clipped.iter().map(|(x, _)| (*x, y_max)).collect();
        let submit: Vec<(f64, f64)> = submit.into_iter().map(|(x, p)| (x, p.min(y_max))).collect();
        let guides: Vec<[(f64, f64); 2]> = (1..periods)
            .map(|m| {
                let y = m as f64 * refresh;
                [(x_bounds[0], y), (x_bounds[1], y)]
            })
            .collect();

        let mut datasets: Vec<Dataset> = guides
            .iter()
            .map(|g| {
                Dataset::default()
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::new().fg(Color::DarkGray))
                    .data(g)
            })
            .collect();
        datasets.push(
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .style(Style::new().fg(Color::Cyan))
                .data(&submit),
        );
        datasets.push(
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .style(Style::new().fg(Color::Yellow))
                .data(&inside),
        );
        datasets.push(
            Dataset::default()
                .marker(symbols::Marker::Block)
                .style(Style::new().fg(Color::Red))
                .data(&clipped),
        );

        let y_labels: Vec<Line> = (0..=periods)
            .step_by(label_step)
            .map(|m| format!("{:.1}", m as f64 * refresh).into())
            .collect();
        let span = self.stats.span().as_secs_f64();
        let title = Line::from(vec![
            " ms ".into(),
            Span::styled("source→present", Color::Yellow),
            " ".into(),
            Span::styled("submit→present", Color::Cyan),
            " ".into(),
            Span::styled("clipped", Color::Red),
            format!(" · guides {refresh:.2} ms · {span:.1} s / {n} frames ").into(),
        ]);
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(title))
            .legend_position(None)
            .x_axis(Axis::default().bounds(x_bounds))
            .y_axis(
                Axis::default()
                    .style(Style::new().fg(Color::Gray))
                    .bounds([0.0, y_max])
                    .labels(y_labels),
            );
        f.render_widget(chart, area);
    }

    fn status(&self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let dim = Style::new().fg(Color::DarkGray);
        let red = Style::new().fg(Color::Red).bold();
        // "key value" with a dim key; groups are separated by two spaces.
        let kv = |key: &str, value: String, style: Style| {
            [
                Span::styled(format!("{key} "), dim),
                Span::styled(format!("{value}  "), style),
            ]
        };
        let plain = |key: &str, value: String| kv(key, value, Style::new());
        let opt = |v: Option<f64>| v.map_or("–".to_owned(), |v| format!("{v:.2}"));
        let percent = |v: Option<f64>| v.map_or("–".to_owned(), |v| format!("{:.0}%", v * 100.0));

        // Line 1: rates, then fixed flag slots on the right.
        let mut rates: Vec<Span> = [
            plain("render", format!("{:.1}", snap.render_hz)),
            plain("capture", format!("{:.1}", snap.capture_hz)),
            plain("skip", format!("{:.1}", snap.skip_hz)),
            plain("miss", format!("{:.1}", snap.miss_hz)),
            plain("drop", format!("{:.1}", snap.capture_drop_hz)),
        ]
        .concat();
        if snap.discard_hz > 0.0 || snap.feedback_timeout_hz > 0.0 {
            rates.extend(kv("discard", format!("{:.1}", snap.discard_hz), red));
            rates.extend(kv(
                "fb timeout",
                format!("{:.1}", snap.feedback_timeout_hz),
                red,
            ));
        }
        rates.push(Span::styled("/s", dim));
        let flag = |name: &str, on: bool, off_alert: bool| {
            let style = match (on, off_alert) {
                (true, _) => Style::new().bold(),
                (false, true) => Style::new().fg(Color::Red).bold(),
                (false, false) => dim,
            };
            Span::styled(format!("  {name}"), style)
        };
        let flags = Line::from(vec![
            flag("confine", snap.confine, false),
            flag("capture", snap.capture, true),
            flag("relative", snap.force_relative, false),
            flag("cursor", snap.cursor_visible, false),
        ])
        .right_aligned();

        // Line 2: the additive latency breakdown and scanout health.
        let alert = |bad: bool| if bad { red } else { Style::new() };
        let feedback_stale = snap.capture && snap.feedback_age_ms.is_some_and(|age| age > 100.0);
        let mut latency: Vec<Span> = vec![
            Span::styled("present ", dim),
            Span::raw(format!(
                "{:.1} ({:.1}–{:.1}) ",
                snap.present_ms, snap.present_min_ms, snap.present_max_ms
            )),
            Span::styled("= age ", dim),
            Span::raw(opt(snap.age_at_grab_ms)),
            Span::styled(" + submit ", dim),
            Span::raw(opt(snap.grab_to_submit_ms)),
            Span::styled(" + out ", dim),
            Span::raw(format!("{} ms  ", opt(snap.submit_to_present_ms))),
        ];
        latency.extend(kv(
            "zero-copy",
            percent(snap.zero_copy_fraction),
            alert(snap.zero_copy_fraction.is_some_and(|z| z < 1.0)),
        ));
        latency.extend(plain("direct", percent(snap.direct_capture_fraction)));
        latency.extend(kv(
            "fb age",
            snap.feedback_age_ms
                .map_or("–".to_owned(), |age| format!("{age:.0} ms")),
            alert(feedback_stale),
        ));
        if feedback_stale {
            latency.push(Span::styled("STALE  ", red));
        }
        if snap.present_frames < snap.frames {
            latency.extend(kv(
                "feedback",
                format!("{}/{}", snap.present_frames, snap.frames),
                Style::new().fg(Color::Yellow),
            ));
        }

        // Line 3: capture-side CPU costs, geometry, uptime.
        let (sw, sh) = snap.source_size;
        let (rw, rh) = snap.render_size;
        let (ww, wh) = snap.window_size;
        let up = self.start.elapsed().as_secs();
        let mut geometry: Vec<Span> = [
            plain("grab wait", format!("{:.2}", snap.wait_ms)),
            plain("copy", format!("{:.2}", snap.obtain_ms)),
            plain("prep", format!("{:.2} ms", snap.commit_ms)),
        ]
        .concat();
        geometry.push(Span::styled(
            format!(
                "· {sw}x{sh} → {rw}x{rh} in {ww}x{wh} window  · up {}:{:02}",
                up / 60,
                up % 60
            ),
            dim,
        ));

        let rows = Layout::vertical([Constraint::Length(1); STATUS_ROWS as usize]).split(area);
        let [rates_area, flags_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(flags.width() as u16)])
                .areas(rows[0]);
        f.render_widget(Paragraph::new(Line::from(rates)), rates_area);
        f.render_widget(Paragraph::new(flags), flags_area);
        f.render_widget(Paragraph::new(Line::from(latency)), rows[1]);
        f.render_widget(Paragraph::new(Line::from(geometry)), rows[2]);
    }

    fn log_paragraph(&self) -> Paragraph<'_> {
        let level_style = |level: Level| match level {
            Level::ERROR => Style::new().fg(Color::Red).bold(),
            Level::WARN => Style::new().fg(Color::Yellow).bold(),
            Level::INFO => Style::new().fg(Color::Green),
            Level::DEBUG => Style::new().fg(Color::Blue),
            Level::TRACE => Style::new().fg(Color::Magenta),
        };
        let lines: Vec<Line> = self
            .logs
            .iter()
            .map(|entry| {
                let at = entry.line.at.saturating_duration_since(self.start);
                let mut spans = vec![
                    Span::styled(
                        format!("{:>7.1} ", at.as_secs_f64()),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:>5} ", entry.line.level),
                        level_style(entry.line.level),
                    ),
                    Span::styled(
                        format!("{}: ", entry.line.target),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::raw(entry.line.message.as_str()),
                ];
                if entry.repeats > 1 {
                    spans.push(Span::styled(
                        format!(" ×{}", entry.repeats),
                        Style::new().fg(Color::Cyan).bold(),
                    ));
                }
                Line::from(spans)
            })
            .collect();
        Paragraph::new(lines).wrap(Wrap { trim: false })
    }

    fn log(&self, f: &mut Frame, area: Rect) {
        let inner_height = area.height.saturating_sub(1) as usize;
        let paragraph = self.log_paragraph();
        let total = paragraph.line_count(area.width);
        let bottom = total.saturating_sub(inner_height);
        let top = self.scroll.map_or(bottom, |row| row.min(bottom));
        let mut block = Block::new()
            .borders(Borders::TOP)
            .title_top(" log · PgUp/PgDn ↑/↓ Home/End scroll · q quit ");
        if self.scroll.is_some() {
            block = block.title_top(
                Line::from(Span::styled(
                    format!(" ↓ {} more ", bottom - top),
                    Style::new().fg(Color::Yellow).bold(),
                ))
                .right_aligned(),
            );
        }
        f.render_widget(paragraph.block(block).scroll((top as u16, 0)), area);
    }
}

fn chart_height(total: u16) -> u16 {
    (total * 45 / 100).clamp(8, 24)
}

fn log_area_height(size: Size) -> usize {
    (size
        .height
        .saturating_sub(chart_height(size.height) + STATUS_ROWS + 1)) as usize
}
