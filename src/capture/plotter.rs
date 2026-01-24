use crate::GlobalState;
use crate::GlobalStateInner;
use crate::capture::nvcapture::FrameInfo;
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

#[derive(Debug, Clone, Copy)]
pub enum EventType {
    Present,
    Capture,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameEvent {
    pub time: Instant,
    pub ty: EventType,
}

#[derive(Debug)]
enum PlotEvent {
    Log(String),
    Draw(FrameTimings),
    Frame(FrameEvent),
    Drop(FrameEvent),
    Fatal(Result<()>),
}

#[derive(Clone)]
pub struct PlotterHandle(mpsc::SyncSender<PlotEvent>);

impl PlotterHandle {
    pub fn log(&self, msg: impl Into<String>) {
        self.0.send(PlotEvent::Log(msg.into())).unwrap();
    }

    pub fn draw(&self, timings: FrameTimings) {
        let _ = self.0.try_send(PlotEvent::Draw(timings));
    }

    pub fn frame(&self, ty: EventType) {
        let _ = self.0.try_send(PlotEvent::Frame(FrameEvent {
            time: Instant::now(),
            ty,
        }));
    }

    pub fn drop(&self, ty: EventType) {
        let _ = self.0.try_send(PlotEvent::Drop(FrameEvent {
            time: Instant::now(),
            ty,
        }));
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
                    PlotEvent::Draw(_) => {} // Ignore draw events when not running in terminal mode
                    PlotEvent::Frame(_) => {}
                    PlotEvent::Drop(_) => {}
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
                PlotEvent::Draw(_) | PlotEvent::Frame(_) | PlotEvent::Drop(_) => continue,
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

#[derive(Debug)]
pub struct FrameTimings {
    start: Instant,
    capture: f64,
    wait: f64,
    cuda: f64,
    commit: f64,
    info: FrameInfo,
}

impl FrameTimings {
    pub fn new(
        start: Instant,
        capture: Duration,
        wait: Duration,
        cuda: Duration,
        info: FrameInfo,
    ) -> Self {
        Self {
            start,
            capture: capture.as_secs_f64() * 1000.0,
            wait: wait.as_secs_f64() * 1000.0,
            cuda: cuda.as_secs_f64() * 1000.0,
            commit: 0.0,
            info,
        }
    }

    pub fn mark_commit(&mut self) {
        self.commit = self.start.elapsed().as_secs_f64() * 1000.0;
    }
}

const DENSITY_FACTOR: usize = 8;
const SCROLL_FROM_START: bool = true;

struct App {
    logs: Vec<String>,
    timings: VecDeque<FrameTimings>,
    delay: Duration,
    log_scroll: usize,
    global_state: GlobalState,
    sizer: SharedSizer,
    present_frames: VecDeque<Instant>,
    capture_frames: VecDeque<Instant>,
    present_drops: VecDeque<Instant>,
    capture_drops: VecDeque<Instant>,
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
            present_frames: VecDeque::new(),
            capture_frames: VecDeque::new(),
            present_drops: VecDeque::new(),
            capture_drops: VecDeque::new(),
        }
    }

    fn timings_avg(&self) -> (f64, f64, f64, f64) {
        if self.timings.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let (capture, wait, cuda, commit) = self
            .timings
            .iter()
            .fold((0.0, 0.0, 0.0, 0.0), |(cap, w, cu, cm), t| {
                (cap + t.capture, w + t.wait, cu + t.cuda, cm + t.commit)
            });
        let len = self.timings.len() as f64;
        (capture / len, wait / len, cuda / len, commit / len)
    }

    fn timings_commit_min_max(&self) -> (f64, f64) {
        if self.timings.is_empty() {
            return (0.0, 0.0);
        }
        self.timings
            .iter()
            .map(|t| t.commit)
            .fold((f64::MAX, f64::MIN), |(min, max), v| (min.min(v), max.max(v)))
    }

    fn timings_fps(&self) -> f64 {
        let Some(now) = self.timings.back() else {
            return 0.0;
        };
        let recent_frames: Vec<_> = self
            .timings
            .iter()
            .rev()
            .take_while(|t| now.start.duration_since(t.start) <= Duration::from_secs(2))
            .collect();

        if recent_frames.len() < 2 {
            return 0.0;
        }

        let oldest = recent_frames.last().unwrap().start;
        let newest = recent_frames.first().unwrap().start;
        let duration = newest.duration_since(oldest);

        let seconds = duration.as_secs_f64();
        if seconds < 1e-9 {
            return 0.0;
        }

        (recent_frames.len() - 1) as f64 / seconds
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
            trim_old(&mut self.present_frames);
            trim_old(&mut self.capture_frames);
            trim_old(&mut self.present_drops);
            trim_old(&mut self.capture_drops);

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
                    PlotEvent::Draw(timings) => {
                        self.timings.push_back(timings);
                    }
                    PlotEvent::Frame(frame_event) => match frame_event.ty {
                        EventType::Present => self.present_frames.push_back(frame_event.time),
                        EventType::Capture => self.capture_frames.push_back(frame_event.time),
                    },
                    PlotEvent::Drop(drop_event) => match drop_event.ty {
                        EventType::Present => self.present_drops.push_back(drop_event.time),
                        EventType::Capture => self.capture_drops.push_back(drop_event.time),
                    },
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
                    Constraint::Length(1),      // State Chart
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

        let wait_data = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.wait))
            .collect::<Vec<_>>();
        let capture_data = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.capture))
            .collect::<Vec<_>>();
        let cuda_data = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.cuda))
            .collect::<Vec<_>>();
        let commit_data = self
            .timings
            .iter()
            .enumerate()
            .map(|(i, t)| (i as f64 - (self.timings.len() - 1) as f64, t.commit))
            .collect::<Vec<_>>();

        let datasets = vec![
            Dataset::default()
                .name("Capture")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Yellow))
                .data(&capture_data),
            Dataset::default()
                .name("Wait")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Cyan))
                .data(&wait_data),
            Dataset::default()
                .name("Cuda")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Magenta))
                .data(&cuda_data),
            Dataset::default()
                .name("Commit")
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::White))
                .data(&commit_data),
        ];

        let max_timing = self
            .timings
            .iter()
            .map(|t| t.capture.max(t.wait).max(t.cuda).max(t.commit))
            .fold(0.0, f64::max);
        let y_axis_bounds = [0.0, (max_timing * 1.2).max(16.67)];

        let y_labels: Vec<Line> = vec![
            "0.0".into(),
            format!("{:.1}", y_axis_bounds[1] / 2.0).into(),
            format!("{:.1}", y_axis_bounds[1]).into(),
        ];

        let y_axis_width = 5; // Width for Y-axis labels and title
        let chart_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(y_axis_width), Constraint::Min(0)])
            .split(chunks[0]);
        let data_area = chart_chunks[1];

        let sizer = self.sizer.load();
        let source_size = self
            .timings
            .back()
            .map(|t| t.info.size)
            .unwrap_or(sizer.source_size);
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

        let states_meta = [
            "post_processing",
            "direct_capture",
            "cursor_visible",
            "cursor_composited",
        ];

        let state_render_area = Rect {
            x: data_area.x,
            y: chunks[1].y,
            width: data_area.width,
            height: 1,
        };

        let state_vis_width = state_render_area.width as usize;
        let num_slots = (state_vis_width.saturating_sub(1)) * 2;
        let mut bins = vec![[false; 4]; num_slots];
        if !self.timings.is_empty() && history_size > 0 {
            for (i, t) in self.timings.iter().enumerate() {
                let relative_idx = i + history_size - self.timings.len();
                let slot = relative_idx * num_slots / history_size;

                if slot < num_slots {
                    let bools = [
                        t.info.required_post_processing,
                        t.info.direct_capture,
                        t.info.cursor_visible,
                        t.info.cursor_composited,
                    ];
                    for (j, &is_set) in bools.iter().enumerate() {
                        if is_set {
                            bins[slot][j] = true;
                        }
                    }
                }
            }
        }

        let mut braille_chars: String = bins
            .chunks(2)
            .map(|chunk| {
                let left = chunk[0];
                let right = if chunk.len() > 1 {
                    chunk[1]
                } else {
                    [false; 4]
                };

                let mut byte = 0u8;
                if left[0] {
                    byte |= 0x01;
                } // Top-left
                if left[1] {
                    byte |= 0x02;
                } // Middle-top-left
                if left[2] {
                    byte |= 0x04;
                } // Middle-bottom-left
                if left[3] {
                    byte |= 0x40;
                } // Bottom-left
                if right[0] {
                    byte |= 0x08;
                } // Top-right
                if right[1] {
                    byte |= 0x10;
                } // Middle-top-right
                if right[2] {
                    byte |= 0x20;
                } // Middle-bottom-right
                if right[3] {
                    byte |= 0x80;
                } // Bottom-right

                std::char::from_u32(0x2800 + byte as u32).unwrap_or(' ')
            })
            .collect();

        if state_vis_width > 0 {
            braille_chars.push('\u{28FF}');
        }

        let state_vis = Paragraph::new(braille_chars);
        f.render_widget(state_vis, state_render_area);

        let (avg_capture, avg_wait, avg_cuda, avg_commit) = self.timings_avg();
        let (min_commit, max_commit) = self.timings_commit_min_max();
        let render_fps = self.timings_fps();

        let present_fps = if self.present_frames.len() < 2 {
            0.0
        } else {
            let duration = self
                .present_frames
                .back()
                .unwrap()
                .duration_since(*self.present_frames.front().unwrap());
            (self.present_frames.len() - 1) as f64 / duration.as_secs_f64().max(1e-9)
        };

        let capture_fps = if self.capture_frames.len() < 2 {
            0.0
        } else {
            let duration = self
                .capture_frames
                .back()
                .unwrap()
                .duration_since(*self.capture_frames.front().unwrap());
            (self.capture_frames.len() - 1) as f64 / duration.as_secs_f64().max(1e-9)
        };

        let present_drops = self.present_drops.len();
        let capture_drops = self.capture_drops.len();

        let state = self.global_state.load();
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
        let state_display = format!("[{}]", state_tags.join(" "));

        let status_text = format!(
            "FPS: R {:.1}, P {:.1}, C {:.1} | Drops: P {}, C {} {} | Capture: {:.2}ms, Wait: {:.2}ms, Cuda: {:.2}ms, Commit: {:.2}ms (min: {:.2}, max: {:.2})",
            render_fps,
            present_fps,
            capture_fps,
            present_drops,
            capture_drops,
            state_display,
            avg_capture,
            avg_wait,
            avg_cuda,
            avg_commit,
            min_commit,
            max_commit
        );
        let status = Paragraph::new(status_text);
        f.render_widget(status, chunks[2]);

        let log_block = Block::default();
        let log_inner_area = log_block.inner(chunks[3]);
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
        f.render_widget(log, chunks[3]);

        let legend_indicators = ['\u{28B9}', '\u{28BA}', '\u{28BC}', '\u{28F8}'];
        let legend_lines: Vec<Line> = states_meta
            .iter()
            .zip(legend_indicators)
            .map(|(name, indicator)| {
                Line::from(vec![Span::raw(format!("{indicator} ")), Span::raw(*name)])
            })
            .collect();

        let legend = Paragraph::new(legend_lines);
        let legend_width = states_meta
            .iter()
            .map(|name| name.len() + 2)
            .max()
            .unwrap_or(0) as u16;
        let legend_height = states_meta.len() as u16;
        let legend_area = Rect {
            x: log_inner_area.right() - legend_width,
            y: log_inner_area.y,
            width: legend_width,
            height: legend_height,
        };
        f.render_widget(legend, legend_area);
    }
}
