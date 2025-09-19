use arc_swap::ArcSwap;
use std::sync::Arc;
use tinyvec::ArrayVec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sizing {
    pub content: Rect,
    pub opaque: ArrayVec<[Rect; 2]>,
}

pub type SharedSizer = Arc<ArcSwap<Sizer>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sizer {
    pub source_size: (u32, u32),
    pub window_size: (u32, u32),
    pub render_size: (u32, u32),
    pub scale120: u32,
    pub window_sizing: Sizing,
    pub render_sizing: Sizing,
}

impl Default for Sizer {
    fn default() -> Self {
        let mut this = Self {
            source_size: (1, 1),
            window_size: (1, 1),
            render_size: (1, 1),
            scale120: 120,
            window_sizing: Sizing::default(),
            render_sizing: Sizing::default(),
        };
        this.recompute();
        this
    }
}

impl Sizer {
    pub fn ready(&self) -> bool {
        self.source_size != (0, 0) && self.window_size != (0, 0)
    }

    /// Scale a mouse delta
    pub fn window_to_source_delta(&self, (dx, dy): (f64, f64)) -> (f64, f64) {
        let content = self.window_sizing.content;
        let dx = dx * (self.source_size.0 as f64 / content.width as f64);
        let dy = dy * (self.source_size.1 as f64 / content.height as f64);
        (dx, dy)
    }

    /// LERP a pixel position, if it's in content area
    pub fn window_to_source(&self, (w, h): (u32, u32)) -> Option<(u32, u32)> {
        let content = self.window_sizing.content;
        if w < content.x
            || w >= content.x + content.width
            || h < content.y
            || h >= content.y + content.height
        {
            return None;
        }

        let x_in_content = w - content.x;
        let y_in_content = h - content.y;

        let source_x =
            (x_in_content as f64 / content.width as f64 * self.source_size.0 as f64).round() as u32;
        let source_y = (y_in_content as f64 / content.height as f64 * self.source_size.1 as f64)
            .round() as u32;

        Some((source_x, source_y))
    }

    pub fn with_window_size(&self, window_size: (u32, u32), scale120: u32) -> Self {
        let mut new_sizer = self.clone();
        new_sizer.window_size = window_size;
        new_sizer.scale120 = scale120;
        new_sizer.recompute();
        new_sizer
    }

    pub fn with_source_size(&self, source_size: (u32, u32)) -> Self {
        let mut new_sizer = self.clone();
        new_sizer.source_size = source_size;
        new_sizer.recompute();
        new_sizer
    }

    fn recompute(&mut self) {
        let (w, h) = self.window_size;
        self.render_size = (w * self.scale120 / 120, h * self.scale120 / 120);
        self.window_sizing = self.calculate_sizing(self.window_size);
        self.render_sizing = self.calculate_sizing(self.render_size);
    }

    fn calculate_sizing(&self, target_size: (u32, u32)) -> Sizing {
        let (src_w, src_h) = self.source_size;
        let (win_w, win_h) = target_size;

        if src_w == 0 || src_h == 0 || win_w == 0 || win_h == 0 {
            return Sizing::default();
        }

        let win_w_f = win_w as f32;
        let win_h_f = win_h as f32;
        let src_w_f = src_w as f32;
        let src_h_f = src_h as f32;

        let win_ar = win_w_f / win_h_f;
        let src_ar = src_w_f / src_h_f;

        let (scaled_w, scaled_h) = if src_ar > win_ar {
            // Letterbox
            let scale = win_w_f / src_w_f;
            (win_w, (src_h_f * scale).round() as u32)
        } else {
            // Pillarbox
            let scale = win_h_f / src_h_f;
            ((src_w_f * scale).round() as u32, win_h)
        };

        let scaled_x = (win_w - scaled_w) / 2;
        let scaled_y = (win_h - scaled_h) / 2;

        let content = Rect {
            x: scaled_x,
            y: scaled_y,
            width: scaled_w,
            height: scaled_h,
        };

        let mut opaque = ArrayVec::new();
        if scaled_w < win_w {
            // Pillarbox
            if scaled_x > 0 {
                opaque.push(Rect {
                    x: 0,
                    y: 0,
                    width: scaled_x,
                    height: win_h,
                });
            }
            let right_bar_x = scaled_x + scaled_w;
            if right_bar_x < win_w {
                opaque.push(Rect {
                    x: right_bar_x,
                    y: 0,
                    width: win_w - right_bar_x,
                    height: win_h,
                });
            }
        } else if scaled_h < win_h {
            // Letterbox
            if scaled_y > 0 {
                opaque.push(Rect {
                    x: 0,
                    y: 0,
                    width: win_w,
                    height: scaled_y,
                });
            }
            let bottom_bar_y = scaled_y + scaled_h;
            if bottom_bar_y < win_h {
                opaque.push(Rect {
                    x: 0,
                    y: bottom_bar_y,
                    width: win_w,
                    height: win_h - bottom_bar_y,
                });
            }
        }

        Sizing { content, opaque }
    }
}
