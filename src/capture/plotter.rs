use crate::GlobalState;
use crate::GlobalStateInner;
use crate::sizer::SharedSizer;
use anyhow::Result;
use arc_swap::ArcSwap;
use crossterm::event::{self, Event, KeyCode};
use ratatui::DefaultTerminal;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
        self.0.send(PlotEvent::Log(msg.into())).unwrap();
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

    pub fn fatal(&self, res: Result<()>) {
        let _ = self.0.send(PlotEvent::Fatal(res));
    }
}

pub struct Plotter {
    handle: PlotterHandle,
    rx: mpsc::Receiver<PlotEvent>,
    global_state: Arc<ArcSwap<GlobalStateInner>>,
    sizer: SharedSizer,
}

impl Plotter {
    pub fn new(global_state: Arc<ArcSwap<GlobalStateInner>>, sizer: SharedSizer) -> Self {
        let (tx, rx) = mpsc::sync_channel(1024);
        Self {
            handle: PlotterHandle(tx),
            rx,
            global_state,
            sizer,
        }
    }

    pub fn run(self, delay: Option<Duration>) -> Result<()> {
        let mut res = Ok(());
        if let Some(delay) = delay {
            let mut app = App::new(delay, self.global_state.clone(), self.sizer.clone());
            let terminal = ratatui::init();
            let app_result = app.run(&self.rx, terminal);
            ratatui::restore();
            res = app_result;
        } else {
            for event in &self.rx {
                match event {
                    PlotEvent::Log(msg) => {
                        let msg = msg.trim_end_matches('\n');
                        println!("{msg}");
                    }
                    PlotEvent::Render(_) => {}
                    PlotEvent::Skip(_) => {}
                    PlotEvent::Capture(_) => {}
                    PlotEvent::CaptureMiss(_) => {}
                    PlotEvent::Fatal(r) => {
                        res = r;
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
        let mut printed = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                PlotEvent::Render(_)
                | PlotEvent::Skip(_)
                | PlotEvent::Capture(_)
                | PlotEvent::CaptureMiss(_) => continue,
                PlotEvent::Fatal(Ok(())) => continue,
                _ => {}
            }
            if !printed {
                printed = true;
                eprintln!("draining plotter queue on the floor:");
            }
            eprintln!("{event:#?}");
        }
        res
    }

    pub fn handle(&self) -> PlotterHandle {
        self.handle.clone()
    }
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub start: Instant,
    pub wait: Instant,
    pub obtain: Instant,
    pub commit: Option<Instant>,
    pub present: Option<Instant>,
    pub cursor_visible: bool,
}

impl FrameInfo {
    pub fn mark_commit(&mut self) {
        self.commit = Some(Instant::now());
    }

    pub fn mark_present(&mut self) {
        self.present = Some(Instant::now());
    }

    fn wait_ms(&self) -> f64 {
        self.wait.duration_since(self.start).as_secs_f64() * 1000.0
    }

    fn obtain_ms(&self) -> f64 {
        self.obtain.duration_since(self.start).as_secs_f64() * 1000.0
    }

    fn commit_ms(&self) -> f64 {
        self.commit
            .map(|c| c.duration_since(self.start).as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    fn present_ms(&self) -> f64 {
        self.present
            .map(|p| p.duration_since(self.start).as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }
}

const DENSITY_FACTOR: usize = 8;
const SCROLL_FROM_START: bool = true;

struct App {
    logs: Vec<String>,
    timings: VecDeque<FrameInfo>,
    delay: Duration,
    log_scroll: usize,
    global_state: GlobalState,
    sizer: SharedSizer,
    captures: VecDeque<Instant>,
    capture_misses: VecDeque<Instant>,
    skips: VecDeque<Instant>,
}

impl App {
    fn new(delay: Duration, global_state: GlobalState, sizer: SharedSizer) -> Self {
        Self {
            logs: Vec::new(),
            timings: VecDeque::new(),
            delay,
            log_scroll: 0,
            global_state,
            sizer,
            captures: VecDeque::new(),
            capture_misses: VecDeque::new(),
            skips: VecDeque::new(),
        }
    }

    fn timings_avg(&self) -> (f64, f64, f64, f64) {
        if self.timings.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let (wait, obtain, commit, present) =
            self.timings
                .iter()
                .fold((0.0, 0.0, 0.0, 0.0), |(w, o, c, p), t| {
                    (
                        w + t.wait_ms(),
                        o + t.obtain_ms(),
                        c + t.commit_ms(),
                        p + t.present_ms(),
                    )
                });
        let len = self.timings.len() as f64;
        (wait / len, obtain / len, commit / len, present / len)
    }

    fn timings_present_min_max(&self) -> (f64, f64) {
        if self.timings.is_empty() {
            return (0.0, 0.0);
        }
        self.timings
            .iter()
            .filter(|t| t.present.is_some())
            .map(|t| t.present_ms())
            .fold((f64::MAX, f64::MIN), |(min, max), v| {
                (min.min(v), max.max(v))
            })
    }

    fn timings_fps(&self) -> Option<f64> {
        let now = self.timings.back()?;
        let (count, oldest) = self
            .timings
            .iter()
            .rev()
            .take_while(|t| now.start.duration_since(t.start) <= Duration::from_secs(2))
            .fold((0, now.start), |(n, _), t| (n + 1, t.start));
        let seconds = now.start.duration_since(oldest).as_secs_f64();
        (count > 1 && seconds > 1e-9).then(|| (count - 1) as f64 / seconds)
    }

    fn run(&mut self, rx: &mpsc::Receiver<PlotEvent>, mut terminal: DefaultTerminal) -> Result<()> {
        loop {
            let history_size = terminal.size()?.width.saturating_sub(5) as usize * DENSITY_FACTOR;
            while self.timings.len() > history_size {
                self.timings.pop_front();
            }

            let two_seconds_ago = Instant::now() - Duration::from_secs(2);
            let trim_old = |q: &mut VecDeque<Instant>| {
                while q.front().is_some_and(|t| *t < two_seconds_ago) {
                    q.pop_front();
                }
            };
            trim_old(&mut self.captures);
            trim_old(&mut self.capture_misses);
            trim_old(&mut self.skips);

            terminal.draw(|f| self.ui(f, history_size))?;

            if event::poll(self.delay)?
                && let Event::Key(key) = event::read()?
            {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::PageUp => {
                        let terminal_width = terminal.size()?.width as usize;
                        let wrapped_logs_len = self
                            .logs
                            .iter()
                            .map(|m| m.len().div_ceil(terminal_width))
                            .sum::<usize>();
                        let terminal_height = terminal.size()?.height as usize;
                        let log_height = terminal_height.saturating_sub(4);
                        let max_scroll = wrapped_logs_len.saturating_sub(log_height);
                        self.log_scroll = (self.log_scroll + 10).min(max_scroll);
                    }
                    KeyCode::PageDown => {
                        self.log_scroll = self.log_scroll.saturating_sub(10);
                    }
                    _ => {}
                }
            }

            // Handle channel events
            for event in rx.try_iter() {
                match event {
                    PlotEvent::Log(msg) => {
                        let msg = msg.trim_end_matches('\n');
                        self.logs.extend(msg.split('\n').map(str::to_owned));
                        self.log_scroll = 0;
                    }
                    PlotEvent::Render(info) => {
                        self.timings.push_back(info);
                    }
                    PlotEvent::Skip(t) => {
                        self.skips.push_back(t);
                    }
                    PlotEvent::Capture(t) => {
                        self.captures.push_back(t);
                    }
                    PlotEvent::CaptureMiss(t) => {
                        self.capture_misses.push_back(t);
                    }
                    PlotEvent::Fatal(res) => {
                        return res;
                    }
                }
            }
        }
    }

    fn ui(&mut self, f: &mut Frame, history_size: usize) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(
                [
                    Constraint::Percentage(50), // Chart
                    Constraint::Length(1),      // Status
                    Constraint::Min(7),         // Log
                ]
                .as_ref(),
            )
            .split(f.area());

        let lower_bound = if SCROLL_FROM_START {
            -(history_size as f64 - 1.0)
        } else {
            -(self.timings.len() as f64 - 1.0)
        };
        let x_axis_bounds = [lower_bound, 0.0];

        let wait_data: Vec<_> = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.wait_ms()))
            .collect();
        let obtain_data: Vec<_> = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.obtain_ms()))
            .collect();
        let commit_data: Vec<_> = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.commit_ms()))
            .collect();
        let present_data: Vec<_> = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.present_ms()))
            .collect();

        let datasets = vec![
            Dataset::default()
                .name("Wait")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Cyan))
                .data(&wait_data),
            Dataset::default()
                .name("Obtain")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Magenta))
                .data(&obtain_data),
            Dataset::default()
                .name("Commit")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::White))
                .data(&commit_data),
            Dataset::default()
                .name("Present")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Yellow))
                .data(&present_data),
        ];

        let max_timing = self
            .timings
            .iter()
            .map(|t| {
                t.wait_ms()
                    .max(t.obtain_ms())
                    .max(t.commit_ms())
                    .max(t.present_ms())
            })
            .fold(0.0, f64::max);
        let y_axis_bounds = [0.0, (max_timing * 1.2).max(16.67)];

        let y_labels: Vec<Line> = vec![
            "0.0".into(),
            format!("{:.1}", y_axis_bounds[1] / 2.0).into(),
            format!("{:.1}", y_axis_bounds[1]).into(),
        ];

        let sizer = self.sizer.load();
        let source_size = sizer.source_size;
        let render_size = sizer.render_size;
        let chart_title = format!(
            "Frame timings {}x{} -> {}x{}",
            source_size.0, source_size.1, render_size.0, render_size.1
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

        let (avg_wait, avg_obtain, avg_commit, avg_present) = self.timings_avg();
        let (min_present, max_present) = self.timings_present_min_max();
        let render_fps = self.timings_fps().unwrap_or(0.0);

        let capture_fps = if self.captures.len() < 2 {
            0.0
        } else {
            let duration = self
                .captures
                .back()
                .unwrap()
                .duration_since(*self.captures.front().unwrap());
            (self.captures.len() - 1) as f64 / duration.as_secs_f64().max(1e-9)
        };

        let skip_count = self.skips.len();
        let miss_count = self.capture_misses.len();

        let state = self.global_state.load();
        let cursor_visible = self.timings.back().is_some_and(|t| t.cursor_visible);
        let mut state_tags = Vec::new();
        if state.confine {
            state_tags.push("G");
        }
        if state.capture {
            state_tags.push("C");
        }
        if state.force_relative {
            state_tags.push("FR");
        }
        if cursor_visible {
            state_tags.push("Cur");
        }
        let state_display = format!("[{}]", state_tags.join(" "));

        let status_text = format!(
            "R{:.1} C{:.1} | S:{} M:{} {} | W:{:.2} O:{:.2} C:{:.2} P:{:.2} ({:.2}-{:.2})",
            render_fps,
            capture_fps,
            skip_count,
            miss_count,
            state_display,
            avg_wait,
            avg_obtain,
            avg_commit,
            avg_present,
            min_present,
            max_present
        );
        let status = Paragraph::new(status_text);
        f.render_widget(status, chunks[1]);

        let log_block = Block::default();
        let log_inner_area = log_block.inner(chunks[2]);
        let log_height = log_inner_area.height as usize;
        let wrapped_logs: Vec<String> = self
            .logs
            .iter()
            .flat_map(|m| {
                let width = log_inner_area.width as usize;
                if m.len() <= width {
                    vec![m.clone()]
                } else {
                    m.chars()
                        .collect::<Vec<_>>()
                        .chunks(width)
                        .map(|chunk| chunk.iter().collect::<String>())
                        .collect()
                }
            })
            .collect();

        let start_idx = if wrapped_logs.len() > log_height {
            if self.log_scroll >= wrapped_logs.len() {
                0
            } else {
                wrapped_logs
                    .len()
                    .saturating_sub(log_height + self.log_scroll)
            }
        } else {
            0
        };

        let log_lines: Vec<ListItem> = wrapped_logs
            .iter()
            .skip(start_idx)
            .take(log_height)
            .map(|m| ListItem::new(Line::from(m.as_str())))
            .collect();
        let log = List::new(log_lines).block(log_block);
        f.render_widget(log, chunks[2]);
    }
}
