//! Rendering: convert the frozen screencopy into a tiny-skia pixmap, draw pen
//! strokes on a transparent overlay, composite, and write back to SHM.

use std::path::Path;
use tiny_skia::{
    LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};
use wayland_client::protocol::wl_shm::Format;
use wayland_client::WEnum;

/// Raw pixels captured from the screen (SHM buffer contents).
#[derive(Clone)]
pub struct CapturedImage {
    pub data: Vec<u8>,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub y_invert: bool,
}

impl CapturedImage {
    pub fn format_value(format: WEnum<Format>) -> Option<Format> {
        match format {
            WEnum::Value(f) => Some(f),
            WEnum::Unknown(_) => None,
        }
    }

    /// Convert to a premultiplied-RGBA pixmap at native capture resolution.
    /// Returns None for unsupported formats.
    pub fn to_pixmap(&self) -> Option<Pixmap> {
        let mut pm = Pixmap::new(self.width, self.height)?;
        let dst = pm.data_mut();
        let w = self.width as usize;
        let h = self.height as usize;
        let stride = self.stride as usize;
        for y in 0..h {
            let sy = if self.y_invert { h - 1 - y } else { y };
            let src_row = &self.data[sy * stride..sy * stride + w * 4];
            let dst_row = &mut dst[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let s = &src_row[x * 4..x * 4 + 4];
                // wl_shm 32-bit formats, little-endian byte order in memory:
                //   argb8888/xrgb8888 -> [B, G, R, A], abgr8888/xbgr8888 -> [R, G, B, A]
                let (r, g, b, a) = match self.format {
                    Format::Argb8888 => (s[2], s[1], s[0], s[3]),
                    Format::Xrgb8888 => (s[2], s[1], s[0], 255),
                    Format::Abgr8888 => (s[0], s[1], s[2], s[3]),
                    Format::Xbgr8888 => (s[0], s[1], s[2], 255),
                    _ => return None,
                };
                let (r, g, b) = if a == 255 {
                    (r, g, b)
                } else {
                    (
                        (r as u32 * a as u32 / 255) as u8,
                        (g as u32 * a as u32 / 255) as u8,
                        (b as u32 * a as u32 / 255) as u8,
                    )
                };
                let d = &mut dst_row[x * 4..x * 4 + 4];
                d[0] = r;
                d[1] = g;
                d[2] = b;
                d[3] = a;
            }
        }
        Some(pm)
    }
}

/// A dirty rectangle in buffer pixel coordinates (x, y, w, h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// One recorded pen segment, buffer pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub pen: f32,
    pub color: [u8; 4],
}

/// One undoable action.
pub enum Action {
    /// All segments of one drag (button press -> release).
    Draw(Vec<Seg>),
    /// A full clear; undoing it restores everything drawn before it.
    Clear,
}

/// A drawing surface: the frozen background with strokes painted directly on
/// top. Segments are rendered into a small scratch pixmap bounded to the
/// stroke's bounding box and blitted back, so a pointer motion only touches a
/// handful of pixels instead of recompositing the whole frame.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    bg: Pixmap,
    frame: Pixmap,
    history: Vec<Action>,
    /// Actions removed by undo, restored by redo.
    redo_stack: Vec<Action>,
    /// Segments of the drag in progress (since button press).
    current: Option<Vec<Seg>>,
}

/// Paint one segment into `frame` (buffer pixels), clipped to the canvas.
fn paint_seg(frame: &mut Pixmap, seg: &Seg, width: u32, height: u32) -> Option<DirtyRect> {
    let margin = seg.pen.max(0.5) / 2.0 + 3.0;
    let min_x = (seg.x0.min(seg.x1) - margin).floor().max(0.0) as i32;
    let min_y = (seg.y0.min(seg.y1) - margin).floor().max(0.0) as i32;
    let max_x = (seg.x0.max(seg.x1) + margin).ceil().min(width as f32) as i32;
    let max_y = (seg.y0.max(seg.y1) + margin).ceil().min(height as f32) as i32;
    let (bw, bh) = (max_x - min_x, max_y - min_y);
    if bw <= 0 || bh <= 0 {
        return None;
    }
    let mut scratch = Pixmap::new(bw as u32, bh as u32)?;
    let mut pb = PathBuilder::new();
    pb.move_to(seg.x0 - min_x as f32, seg.y0 - min_y as f32);
    pb.line_to(seg.x1 - min_x as f32, seg.y1 - min_y as f32);
    let path = pb.finish()?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(seg.color[0], seg.color[1], seg.color[2], seg.color[3]);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: seg.pen.max(0.5),
        miter_limit: 4.0,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        dash: None,
    };
    scratch
        .as_mut()
        .stroke_path(&path, &paint, &stroke, Transform::default(), None);
    frame.as_mut().draw_pixmap(
        min_x,
        min_y,
        scratch.as_ref(),
        &PixmapPaint::default(),
        Transform::default(),
        None,
    );
    Some(DirtyRect { x: min_x, y: min_y, w: bw as u32, h: bh as u32 })
}

impl Canvas {
    /// Build a canvas of `width`x`height` (buffer pixels) from a captured image,
    /// scaling the image if the sizes differ.
    pub fn new(img: &CapturedImage, width: u32, height: u32) -> Option<Canvas> {
        let src = img.to_pixmap()?;
        let mut frame = Pixmap::new(width, height)?;
        let sx = width as f32 / img.width as f32;
        let sy = height as f32 / img.height as f32;
        frame.as_mut().draw_pixmap(
            0,
            0,
            src.as_ref(),
            &PixmapPaint::default(),
            Transform::from_scale(sx, sy),
            None,
        );
        let bg = frame.clone();
        Some(Canvas {
            width,
            height,
            bg,
            frame,
            history: Vec::new(),
            redo_stack: Vec::new(),
            current: None,
        })
    }

    /// Start a new undo step (called on button press). Any new action
    /// invalidates the redo history.
    pub fn begin_step(&mut self) {
        self.end_step(); // commit a dangling step if any
        self.redo_stack.clear();
        self.current = Some(Vec::new());
    }

    /// Commit the drag in progress to the undo history (button release).
    pub fn end_step(&mut self) {
        if let Some(step) = self.current.take() {
            if !step.is_empty() {
                self.history.push(Action::Draw(step));
            }
        }
    }

    /// Draw one pen segment (buffer pixel coordinates) and return the region
    /// of the canvas that changed. Returns None if nothing was drawn.
    pub fn stroke_segment(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        pen: f32,
        color: [u8; 4],
    ) -> Option<DirtyRect> {
        let seg = Seg { x0, y0, x1, y1, pen, color };
        if let Some(step) = self.current.as_mut() {
            step.push(seg);
        }
        paint_seg(&mut self.frame, &seg, self.width, self.height)
    }

    /// Erase all strokes, restoring the frozen background. Returns the region
    /// of the canvas that changed (the whole canvas). Recorded in the undo
    /// history: undo right after restores everything. A new clear invalidates
    /// the redo history.
    pub fn clear(&mut self) -> DirtyRect {
        self.end_step();
        self.redo_stack.clear();
        self.history.push(Action::Clear);
        self.frame = self.bg.clone();
        DirtyRect { x: 0, y: 0, w: self.width, h: self.height }
    }

    /// Undo the last action (one drag or one clear). Rebuilds the frame by
    /// replaying the remaining history over the frozen background. Returns
    /// the damaged region, or None if there is nothing left to undo.
    /// The undone action is kept for redo().
    pub fn undo(&mut self) -> Option<DirtyRect> {
        self.end_step();
        let popped = self.history.pop()?;
        self.redo_stack.push(popped);
        self.rebuild_from_history();
        Some(DirtyRect { x: 0, y: 0, w: self.width, h: self.height })
    }

    /// Redo the action most recently undone. Returns the damaged region, or
    /// None if there is nothing to redo.
    pub fn redo(&mut self) -> Option<DirtyRect> {
        self.end_step();
        let popped = self.redo_stack.pop()?;
        self.history.push(popped);
        self.rebuild_from_history();
        Some(DirtyRect { x: 0, y: 0, w: self.width, h: self.height })
    }

    /// frame = background + replay of the full history.
    fn rebuild_from_history(&mut self) {
        let mut frame = self.bg.clone();
        for action in &self.history {
            match action {
                Action::Draw(segs) => {
                    for seg in segs {
                        paint_seg(&mut frame, seg, self.width, self.height);
                    }
                }
                Action::Clear => {
                    frame = self.bg.clone();
                }
            }
        }
        self.frame = frame;
    }

    /// Write the given region (or the whole frame) as XRGB8888 into a SHM
    /// buffer (LE bytes [B,G,R,X]).
    pub fn write_shm_xrgb(&self, dst: &mut [u8], rect: DirtyRect) {
        let src = self.frame.data();
        let stride = self.width as usize * 4;
        for row in 0..rect.h as usize {
            let sy = rect.y as usize + row;
            let sx = rect.x as usize * 4;
            let src_row = &src[sy * stride + sx..sy * stride + sx + rect.w as usize * 4];
            let dst_row = &mut dst[sy * stride + sx..sy * stride + sx + rect.w as usize * 4];
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 0xff;
            }
        }
    }

    pub fn save_png(&self, path: &Path) -> std::io::Result<()> {
        let png = self
            .frame
            .encode_png()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        std::fs::write(path, png)
    }
}

/// Pen cursor: a thin black ring with a white inner ring, transparent
/// center, so the screen content inside the pen footprint stays visible.
pub fn ring_cursor_pixmap(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("cursor pixmap alloc");
    let center = size as f32 / 2.0;
    // outer black ring, following the pen footprint edge
    stroke_circle(&mut pm, center, center, (size as f32 - 2.0) / 2.0, 2.0, [0, 0, 0, 255]);
    // inner white ring just inside it
    stroke_circle(&mut pm, center, center, (size as f32 - 2.0) / 2.0 - 1.0, 1.0, [255, 255, 255, 255]);
    pm
}

fn stroke_circle(pm: &mut Pixmap, cx: f32, cy: f32, radius: f32, width: f32, color: [u8; 4]) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, radius.max(0.5));
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            miter_limit: 4.0,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            dash: None,
        };
        pm.stroke_path(&path, &paint, &stroke, Transform::default(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img_argb(w: u32, h: u32, px: [u8; 4]) -> CapturedImage {
        // px = [B, G, R, A] as stored in an ARGB8888 SHM buffer on LE
        let mut data = vec![0u8; (w * h * 4) as usize];
        for c in data.chunks_exact_mut(4) {
            c.copy_from_slice(&px);
        }
        CapturedImage { data, format: Format::Argb8888, width: w, height: h, stride: w * 4, y_invert: false }
    }

    #[test]
    fn argb_to_pixmap() {
        // opaque red in wl_shm ARGB8888 memory layout: B=0, G=0, R=255, A=255
        let img = img_argb(2, 2, [0, 0, 255, 255]);
        let pm = img.to_pixmap().unwrap();
        assert_eq!(&pm.data()[0..4], &[255, 0, 0, 255]); // premult RGBA
    }

    #[test]
    fn xrgb_is_opaque() {
        let img = img_argb(1, 1, [10, 20, 30, 0]); // alpha byte garbage in XRGB
        let img = CapturedImage { format: Format::Xrgb8888, ..img };
        let pm = img.to_pixmap().unwrap();
        assert_eq!(&pm.data()[0..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn abgr_layout() {
        // ABGR8888 memory layout is already [R, G, B, A]
        let img = CapturedImage {
            data: vec![30, 20, 10, 255],
            format: Format::Abgr8888,
            width: 1,
            height: 1,
            stride: 4,
            y_invert: false,
        };
        let pm = img.to_pixmap().unwrap();
        assert_eq!(&pm.data()[0..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn premultiply() {
        let img = img_argb(1, 1, [0, 0, 200, 128]); // half-transparent red
        let pm = img.to_pixmap().unwrap();
        assert_eq!(pm.data()[3], 128);
        assert_eq!(pm.data()[0], (200u32 * 128 / 255) as u8);
    }

    #[test]
    fn y_invert() {
        // 1x2 image, stride 4: row0 = red, row1 = green (in memory order)
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&[0, 0, 255, 255]); // row 0: red
        data[4..8].copy_from_slice(&[0, 255, 0, 255]); // row 1: green
        let img = CapturedImage { data, format: Format::Argb8888, width: 1, height: 2, stride: 4, y_invert: true };
        let pm = img.to_pixmap().unwrap();
        assert_eq!(&pm.data()[0..4], &[0, 255, 0, 255]); // top row should be green
        assert_eq!(&pm.data()[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn canvas_roundtrip() {
        let img = img_argb(4, 4, [0, 0, 255, 255]);
        let mut c = Canvas::new(&img, 4, 4).unwrap();
        let rect = c.stroke_segment(0.0, 0.0, 3.0, 3.0, 2.0, [0, 255, 0, 255]).unwrap();
        // the dirtied region must contain the stroke endpoint
        assert!(rect.x <= 3 && rect.y <= 3 && rect.x + rect.w as i32 > 3 && rect.y + rect.h as i32 > 3);
        let mut shm = vec![0u8; 4 * 4 * 4];
        c.write_shm_xrgb(&mut shm, DirtyRect { x: 0, y: 0, w: 4, h: 4 });
        // pixel (3,3) is on the stroke end: anti-aliased round cap gives partial
        // coverage, so just require it to be overwhelmingly green ([B,G,R,X])
        let p = &shm[3 * 4 * 4 + 3 * 4..3 * 4 * 4 + 3 * 4 + 4];
        assert!(p[1] > 150, "not green-dominant: {p:?}");
        assert!(p[0] + p[2] < 105, "bg bleeding through: {p:?}");
        // line from (0,0)-(3,3) doesn't cover (0,3); that pixel stays red bg
        let p = &shm[0 * 4 * 4 + 3 * 4..0 * 4 * 4 + 3 * 4 + 4];
        assert_eq!(p, &[0, 0, 255, 255]);
    }

    #[test]
    fn dirty_region_only() {
        // writing a small dirty region must leave the rest of the SHM untouched
        let img = img_argb(64, 64, [0, 0, 255, 255]);
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        let mut shm = vec![0u8; 64 * 64 * 4];
        let rect = c.stroke_segment(8.0, 8.0, 8.0, 12.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.write_shm_xrgb(&mut shm, rect);
        // the rect must stay away from the far corner
        assert!(rect.w < 32 && rect.h < 32, "rect too big: {rect:?}");
        // pixel (63,63) is far from the stroke; it was never written
        let idx = 63 * 64 * 4 + 63 * 4;
        assert_eq!(&shm[idx..idx + 4], &[0, 0, 0, 0]);
        // the stroke end itself was written and is green-dominant ([B,G,R,X])
        let idx = 12 * 64 * 4 + 8 * 4;
        let p = &shm[idx..idx + 4];
        assert!(p[1] > 150, "not green-dominant: {p:?}");
    }

    #[test]
    fn clear_works() {
        let img = img_argb(2, 2, [0, 0, 255, 255]);
        let mut c = Canvas::new(&img, 2, 2).unwrap();
        c.stroke_segment(0.0, 0.0, 1.0, 1.0, 2.0, [0, 255, 0, 255]).unwrap();
        let rect = c.clear();
        assert_eq!(rect, DirtyRect { x: 0, y: 0, w: 2, h: 2 });
        assert_eq!(&c.frame.data()[0..4], &[255, 0, 0, 255]);
    }

    fn pixel(pm: &Pixmap, x: usize, y: usize, w: usize) -> [u8; 4] {
        let d = pm.data();
        let i = (y * w + x) * 4;
        [d[i], d[i + 1], d[i + 2], d[i + 3]]
    }

    fn dominated_by(px: [u8; 4], c: [u8; 4]) -> bool {
        // stroke color dominates the AA-blended pixel
        let (sr, sg, sb) = (px[0] as i32, px[1] as i32, px[2] as i32);
        let (cr, cg, cb) = (c[0] as i32, c[1] as i32, c[2] as i32);
        let dist = (sr - cr).abs() + (sg - cg).abs() + (sb - cb).abs();
        dist < 160
    }

    #[test]
    fn undo_removes_last_drag() {
        let img = img_argb(64, 64, [0, 0, 255, 255]); // red bg
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        // drag 1: green stroke on the left
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        // drag 2: blue stroke on the right
        c.begin_step();
        c.stroke_segment(40.0, 40.0, 40.0, 44.0, 2.0, [0, 0, 255, 255]).unwrap();
        c.stroke_segment(40.0, 40.0, 40.0, 44.0, 2.0, [0, 0, 255, 255]).unwrap();
        c.end_step();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]));
        assert!(dominated_by(pixel(&c.frame, 40, 42, 64), [0, 0, 255, 255]));
        // undo removes drag 2 only
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]),
            "drag 1 must survive: {:?}", pixel(&c.frame, 10, 12, 64));
        assert!(!dominated_by(pixel(&c.frame, 40, 42, 64), [0, 0, 255, 255]),
            "drag 2 must be gone: {:?}", pixel(&c.frame, 40, 42, 64));
        // undo removes drag 1
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]));
        // nothing left to undo
        assert!(c.undo().is_none());
    }

    #[test]
    fn undo_clear_restores() {
        let img = img_argb(64, 64, [0, 0, 255, 255]); // red bg
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        c.clear();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]),
            "clear must reset to bg");
        // undo the clear: the stroke comes back
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]),
            "undo must restore the stroke: {:?}", pixel(&c.frame, 10, 12, 64));
    }

    #[test]
    fn undo_during_drag_commits_it_first() {
        // pressing undo while a drag is still open commits that drag, then
        // undoes it right away (net effect: nothing drawn)
        let img = img_argb(64, 64, [0, 0, 255, 255]);
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        // no end_step(): undo called mid-drag
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]),
            "mid-drag undo should leave no stroke: {:?}", pixel(&c.frame, 10, 12, 64));
    }

    #[test]
    fn click_dot_is_one_step() {
        // a plain click draws a dot; one undo removes it
        let img = img_argb(64, 64, [0, 0, 255, 255]);
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(30.0, 30.0, 30.01, 30.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        assert!(dominated_by(pixel(&c.frame, 30, 30, 64), [0, 255, 0, 255]));
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 30, 30, 64), [255, 0, 0, 255]));
        assert!(c.undo().is_none());
    }

    #[test]
    fn redo_restores_undone() {
        let img = img_argb(64, 64, [0, 0, 255, 255]); // red bg
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        c.undo();
        assert!(c.undo().is_none());
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]));
        // redo brings the drag back, pixel-identical
        c.redo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]));
        assert!(c.redo().is_none(), "nothing left to redo");
        // undo/undo/redo redo ordering stays consistent
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]));
        c.redo();
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]));
    }

    #[test]
    fn new_draw_invalidates_redo() {
        let img = img_argb(64, 64, [0, 0, 255, 255]); // red bg
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        // drag 2
        c.begin_step();
        c.stroke_segment(40.0, 40.0, 40.0, 44.0, 2.0, [0, 0, 255, 255]).unwrap();
        c.end_step();
        // undo both
        c.undo();
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 40, 42, 64), [255, 0, 0, 255]));
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]));
        // draw something new: redo history is gone
        c.begin_step();
        c.stroke_segment(20.0, 20.0, 20.0, 24.0, 2.0, [255, 255, 0, 255]).unwrap();
        c.end_step();
        assert!(c.redo().is_none(), "redo must be invalid after a new draw");
        assert!(dominated_by(pixel(&c.frame, 20, 22, 64), [255, 255, 0, 255]));
        // undo still works on the new step
        c.undo();
        assert!(dominated_by(pixel(&c.frame, 20, 22, 64), [255, 0, 0, 255]));
    }

    #[test]
    fn redo_clear() {
        let img = img_argb(64, 64, [0, 0, 255, 255]); // red bg
        let mut c = Canvas::new(&img, 64, 64).unwrap();
        c.begin_step();
        c.stroke_segment(10.0, 10.0, 10.0, 14.0, 2.0, [0, 255, 0, 255]).unwrap();
        c.end_step();
        c.clear();
        c.undo(); // undo the clear
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [0, 255, 0, 255]));
        c.redo(); // redo the clear
        assert!(dominated_by(pixel(&c.frame, 10, 12, 64), [255, 0, 0, 255]),
            "redo of a clear must wipe again");
        assert!(c.redo().is_none());
    }
}
