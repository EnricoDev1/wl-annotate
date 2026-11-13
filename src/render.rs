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

/// A drawing surface: the frozen background with strokes painted directly on
/// top. Segments are rendered into a small scratch pixmap bounded to the
/// stroke's bounding box and blitted back, so a pointer motion only touches a
/// handful of pixels instead of recompositing the whole frame.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    bg: Pixmap,
    frame: Pixmap,
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
        Some(Canvas { width, height, bg, frame })
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
        let margin = pen.max(0.5) / 2.0 + 3.0;
        let min_x = (x0.min(x1) - margin).floor().max(0.0) as i32;
        let min_y = (y0.min(y1) - margin).floor().max(0.0) as i32;
        let max_x = (x0.max(x1) + margin).ceil().min(self.width as f32) as i32;
        let max_y = (y0.max(y1) + margin).ceil().min(self.height as f32) as i32;
        let (bw, bh) = (max_x - min_x, max_y - min_y);
        if bw <= 0 || bh <= 0 {
            return None;
        }
        let mut scratch = Pixmap::new(bw as u32, bh as u32)?;
        let mut pb = PathBuilder::new();
        pb.move_to(x0 - min_x as f32, y0 - min_y as f32);
        pb.line_to(x1 - min_x as f32, y1 - min_y as f32);
        let path = pb.finish()?;
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: pen.max(0.5),
            miter_limit: 4.0,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            dash: None,
        };
        scratch
            .as_mut()
            .stroke_path(&path, &paint, &stroke, Transform::default(), None);
        self.frame.as_mut().draw_pixmap(
            min_x,
            min_y,
            scratch.as_ref(),
            &PixmapPaint::default(),
            Transform::default(),
            None,
        );
        Some(DirtyRect { x: min_x, y: min_y, w: bw as u32, h: bh as u32 })
    }

    /// Erase all strokes, restoring the frozen background. Returns the region
    /// of the canvas that changed (the whole canvas).
    pub fn clear(&mut self) -> DirtyRect {
        self.frame = self.bg.clone();
        DirtyRect { x: 0, y: 0, w: self.width, h: self.height }
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
}
