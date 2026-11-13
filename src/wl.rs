//! Wayland client state: registry, screencopy freeze, layer-shell overlay,
//! pointer/keyboard drawing.

use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::ptr::NonNull;
use std::time::{SystemTime, UNIX_EPOCH};

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::unistd::ftruncate;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::config::Config;
use crate::render::{self, Canvas, CapturedImage, DirtyRect};

/// A shared memory mapping of a memfd, unmapped on drop.
pub struct ShmMap {
    ptr: NonNull<u8>,
    len: usize,
}

impl ShmMap {
    fn new(fd: &impl AsFd, len: usize) -> nix::Result<ShmMap> {
        let ptr = unsafe {
            mmap(
                None,
                NonZeroUsize::new(len).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )?
        };
        Ok(ShmMap { ptr: ptr.cast(), len })
    }
}

impl Deref for ShmMap {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for ShmMap {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(self.ptr.cast(), self.len);
        }
    }
}

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
/// In-session evdev key codes (linux/input-event-codes.h).
const KEY_ESC_EVDEV: u32 = 1;
const KEY_Q_EVDEV: u32 = 16;
const KEY_C_EVDEV: u32 = 46;
const KEY_S_EVDEV: u32 = 31;
const KEY_R_EVDEV: u32 = 19;
const KEY_G_EVDEV: u32 = 34;
const KEY_B_EVDEV: u32 = 48;
const KEY_M_EVDEV: u32 = 50;
const KEY_W_EVDEV: u32 = 17;

/// wl_keyboard key events normally carry the evdev code + 8, but some
/// compositors (notably niri) forward the raw evdev code. Accept both.
fn key_is(event_code: u32, evdev_code: u32) -> bool {
    event_code == evdev_code || event_code == evdev_code + 8
}

/// Pen color selected by a number/letter key while drawing.
fn color_key(event_code: u32) -> Option<(&'static str, [u8; 4])> {
    const RED: [u8; 4] = [0xff, 0x00, 0x00, 0xff];
    const GREEN: [u8; 4] = [0x00, 0xff, 0x00, 0xff];
    const BLUE: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
    const MAGENTA: [u8; 4] = [0xff, 0x00, 0xff, 0xff];
    const WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    if key_is(event_code, KEY_R_EVDEV) {
        Some(("red", RED))
    } else if key_is(event_code, KEY_G_EVDEV) {
        Some(("green", GREEN))
    } else if key_is(event_code, KEY_B_EVDEV) {
        Some(("blue", BLUE))
    } else if key_is(event_code, KEY_M_EVDEV) {
        Some(("magenta", MAGENTA))
    } else if key_is(event_code, KEY_W_EVDEV) {
        Some(("white", WHITE))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    On,
    Off,
    Clear,
    Save,
    Status,
    Quit,
}

pub fn parse_command(s: &str) -> Option<Command> {
    match s.trim().to_ascii_lowercase().as_str() {
        "toggle" => Some(Command::Toggle),
        "on" | "start" | "activate" => Some(Command::On),
        "off" | "stop" | "exit" | "deactivate" => Some(Command::Off),
        "clear" => Some(Command::Clear),
        "save" => Some(Command::Save),
        "status" => Some(Command::Status),
        "quit" => Some(Command::Quit),
        _ => None,
    }
}

pub struct Output {
    pub name: u32,
    pub proxy: wl_output::WlOutput,
    pub scale: i32,
    pub description: String,
}

pub struct OutputUData {
    pub name: u32,
}

pub(crate) struct CaptureBuf {
    #[allow(dead_code)] // OwnedFd kept alive so the pool fd stays valid
    fd: OwnedFd,
    map: ShmMap,
    pool: wl_shm_pool::WlShmPool,
    buf: wl_buffer::WlBuffer,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

pub(crate) struct OutBuf {
    map: ShmMap,
    pool: wl_shm_pool::WlShmPool,
    buf: wl_buffer::WlBuffer,
}

/// Kept alive until process exit so no events arrive for destroyed objects.
pub(crate) struct Retired {
    #[allow(dead_code)] // kept alive so the compositors buffers are not freed early
    pool: Option<wl_shm_pool::WlShmPool>,
    #[allow(dead_code)]
    buf: Option<wl_buffer::WlBuffer>,
}

/// A wl_surface used as the pointer cursor: a ring showing the pen size.
/// Rebuilt only when the ring diameter or output scale changes.
pub struct CursorBuf {
    pub surface: wl_surface::WlSurface,
    pool: Option<wl_shm_pool::WlShmPool>,
    buf: Option<wl_buffer::WlBuffer>,
    /// committed buffer side length, device px
    size: u32,
    scale: i32,
}

pub struct Session {
    pub output_name: u32,
    pub frame: Option<ZwlrScreencopyFrameV1>,
    pub cap: Option<CaptureBuf>,
    pub y_invert: bool,
    pub img: Option<CapturedImage>,
    pub wl_surface: Option<wl_surface::WlSurface>,
    pub layer: Option<ZwlrLayerSurfaceV1>,
    pub logical: (u32, u32),
    pub scale: i32,
    pub canvas: Option<Canvas>,
    pub out: Option<OutBuf>,
    pub drawing: bool,
    pub last: Option<(f32, f32)>,
    pub visible: bool,
}

impl Session {
    fn new(output_name: u32, frame: ZwlrScreencopyFrameV1) -> Session {
        Session {
            output_name,
            frame: Some(frame),
            cap: None,
            y_invert: false,
            img: None,
            wl_surface: None,
            layer: None,
            logical: (0, 0),
            scale: 1,
            canvas: None,
            out: None,
            drawing: false,
            last: None,
            visible: false,
        }
    }
}

pub struct State {
    pub config: Config,
    pub qh: QueueHandle<State>,
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub shm: Option<wl_shm::WlShm>,
    pub seat: Option<wl_seat::WlSeat>,
    pub pointer: Option<wl_pointer::WlPointer>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub screencopy: Option<ZwlrScreencopyManagerV1>,
    pub outputs: Vec<Output>,
    pub sessions: Vec<Session>,
    pub retired: Vec<Retired>,
    pub cursor: Option<CursorBuf>,
    pub cursor_serial: Option<u32>,
    pub active: bool,
    pub quit: bool,
    pub pointer_session: Option<usize>,
    pub pointer_pos: Option<(f32, f32)>,
    pub pen: f32,
}

pub fn log(verbose: bool, args: std::fmt::Arguments) {
    if verbose {
        eprintln!("wl-annotate: {args}");
    }
}

impl State {
    pub fn new(config: Config, qh: QueueHandle<State>) -> State {
        let pen = config.pen_size;
        State {
            config,
            qh,
            compositor: None,
            shm: None,
            seat: None,
            pointer: None,
            keyboard: None,
            layer_shell: None,
            screencopy: None,
            outputs: Vec::new(),
            sessions: Vec::new(),
            retired: Vec::new(),
            cursor: None,
            cursor_serial: None,
            active: false,
            quit: false,
            pointer_session: None,
            pointer_pos: None,
            pen,
        }
    }

    fn verbose(&self) -> bool {
        self.config.verbose
    }

    pub fn handle_command(&mut self, cmd: Command) -> String {
        match cmd {
            Command::Toggle => {
                if self.active {
                    self.deactivate();
                    "ok off".to_string()
                } else {
                    self.activate()
                }
            }
            Command::On => self.activate(),
            Command::Off => {
                self.deactivate();
                if self.config.once {
                    self.quit = true;
                }
                "ok off".to_string()
            }
            Command::Clear => {
                self.clear_all();
                "ok".to_string()
            }
            Command::Save => self.save_all(),
            Command::Status => {
                let n = self.sessions.iter().filter(|s| s.visible).count();
                format!("{} ({} output(s) annotated)", if self.active { "active" } else { "inactive" }, n)
            }
            Command::Quit => {
                self.deactivate();
                self.quit = true;
                "ok quit".to_string()
            }
        }
    }

    pub fn activate(&mut self) -> String {
        if self.active {
            return "ok already active".to_string();
        }
        if self.screencopy.is_none() {
            return "error: compositor does not support zwlr_screencopy_manager_v1".to_string();
        }
        if self.layer_shell.is_none() {
            return "error: compositor does not support zwlr_layer_shell_v1".to_string();
        }
        if self.outputs.is_empty() {
            return "error: no outputs found".to_string();
        }
        self.active = true;
        if self.keyboard.is_none() {
            eprintln!("wl-annotate: no keyboard bound; ESC / in-session keys will NOT work");
        }
        let names: Vec<u32> = self.outputs.iter().map(|o| o.name).collect();
        for n in names {
            self.start_capture(n);
        }
        log(self.verbose(), format_args!("activated, freezing {} output(s)", self.sessions.len()));
        "ok on".to_string()
    }

    pub fn deactivate(&mut self) {
        if !self.active && self.sessions.is_empty() {
            return;
        }
        log(self.verbose(), format_args!("deactivating, discarding annotations"));
        let sessions = std::mem::take(&mut self.sessions);
        for mut s in sessions {
            if let Some(layer) = s.layer.take() {
                layer.destroy();
            }
            if let Some(surf) = s.wl_surface.take() {
                surf.destroy();
            }
            if let Some(frame) = s.frame.take() {
                frame.destroy();
            }
            if let Some(out) = s.out.take() {
                self.retired.push(Retired { pool: Some(out.pool), buf: Some(out.buf) });
            }
            if let Some(cap) = s.cap.take() {
                self.retired.push(Retired { pool: Some(cap.pool), buf: Some(cap.buf) });
            }
        }
        self.active = false;
        self.pointer_session = None;
        self.pointer_pos = None;
        if let Some(cursor) = self.cursor.take() {
            cursor.surface.destroy();
            self.retired.push(Retired { pool: cursor.pool, buf: cursor.buf });
        }
        self.cursor_serial = None;
    }

    /// Build/refresh the ring cursor surface and install it as this pointer's
    /// cursor. Called on pointer enter and whenever the pen size changes.
    /// `serial` comes from the last wl_POINTER Enter; stored for later updates.
    fn update_cursor(&mut self, serial: Option<u32>) {
        let Some(serial) = serial.or(self.cursor_serial) else { return };
        self.cursor_serial = Some(serial);
        let Some(pointer) = self.pointer.clone() else { return };
        let scale = self
            .pointer_session
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.scale)
            .unwrap_or(1)
            .max(1);
        let pen = self.pen;
        // ring diameter in device px, with room for the outer stroke
        let d = (pen * scale as f32).round().max(4.0) as u32;
        let size = d + 6;

        let needs_rebuild = match &self.cursor {
            Some(c) => c.size != size || c.scale != scale,
            None => {
                let Some(comp) = self.compositor.clone() else { return };
                self.cursor = Some(CursorBuf {
                    surface: comp.create_surface(&self.qh, ()),
                    pool: None,
                    buf: None,
                    size: 0,
                    scale: 0,
                });
                true
            }
        };

        if needs_rebuild {
            let Some(shm) = self.shm.clone() else { return };
            let pixels = render::ring_cursor_pixmap(size);
            let stride = size as usize * 4;
            let shm_size = stride * size as usize;
            let fd = match memfd_create("wl-annotate-cursor", MFdFlags::MFD_CLOEXEC) {
                Ok(f) => f,
                Err(e) => {
                    log(true, format_args!("cursor memfd_create failed: {e}"));
                    return;
                }
            };
            if let Err(e) = ftruncate(&fd, shm_size as i64) {
                log(true, format_args!("cursor ftruncate failed: {e}"));
                return;
            }
            let mut map = match ShmMap::new(&fd, shm_size) {
                Ok(m) => m,
                Err(e) => {
                    log(true, format_args!("cursor mmap failed: {e}"));
                    return;
                }
            };
            // premultiplied tiny-skia RGBA (R,G,B,A) -> wl_shm ARGB8888 (B,G,R,A)
            for (s, d) in pixels.data().chunks_exact(4).zip(map[..].chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
            let qh = self.qh.clone();
            let pool = shm.create_pool(fd.as_fd(), shm_size as i32, &qh, ());
            let buf = pool.create_buffer(
                0,
                size as i32,
                size as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            );
            let surface = self.cursor.as_ref().unwrap().surface.clone();
            let (old_pool, old_buf) = {
                let c = self.cursor.as_mut().unwrap();
                (c.pool.take(), c.buf.take())
            };
            surface.set_buffer_scale(scale);
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, size as i32, size as i32);
            surface.commit();
            // retire the previous pool/buffer only after the new one is committed
            if old_pool.is_some() || old_buf.is_some() {
                self.retired.push(Retired { pool: old_pool, buf: old_buf });
            }
            *self.cursor.as_mut().unwrap() = CursorBuf {
                surface,
                pool: Some(pool),
                buf: Some(buf),
                size,
                scale,
            };
        }

        let c = self.cursor.as_ref().expect("cursor exists");
        // hotspot: center of the ring, in cursor-surface logical coordinates
        let hotspot = ((size / 2) as f32 / scale as f32).round() as i32;
        pointer.set_cursor(serial, Some(&c.surface), hotspot, hotspot);
        log(true, format_args!("cursor: ring {size}px, pen {pen:.1} (scale {scale})"));
    }

    fn clear_all(&mut self) {
        for i in 0..self.sessions.len() {
            let rect = self.sessions[i].canvas.as_mut().map(|c| c.clear());
            if let Some(rect) = rect {
                self.redraw(i, rect);
            }
        }
    }

    fn save_all(&mut self) -> String {
        let ts = timestamp();
        let dir = self.config.save_dir.clone();
        let mut msgs = Vec::new();
        for i in 0..self.sessions.len() {
            let s = &mut self.sessions[i];
            let Some(canvas) = s.canvas.as_mut() else { continue };
            let path = dir.join(format!("wl-annotate-{}-{}.png", ts, s.output_name));
            match canvas.save_png(&path) {
                Ok(()) => msgs.push(format!("saved {}", path.display())),
                Err(e) => msgs.push(format!("error saving {}: {e}", path.display())),
            }
        }
        if msgs.is_empty() {
            "nothing to save (not active?)".to_string()
        } else {
            msgs.join("\n")
        }
    }

    fn output_proxy(&self, name: u32) -> Option<wl_output::WlOutput> {
        self.outputs.iter().find(|o| o.name == name).map(|o| o.proxy.clone())
    }

    fn start_capture(&mut self, output_name: u32) {
        let Some(sc) = self.screencopy.clone() else { return };
        let Some(output) = self.output_proxy(output_name) else { return };
        let frame = sc.capture_output(0, &output, &self.qh, output_name);
        self.sessions.push(Session::new(output_name, frame));
    }

    fn fail_session(&mut self, idx: usize, msg: &str) {
        log(true, format_args!("output {}: {msg}", self.sessions[idx].output_name));
        if idx < self.sessions.len() {
            let mut s = self.sessions.remove(idx);
            if let Some(frame) = s.frame.take() {
                frame.destroy();
            }
            if let Some(cap) = s.cap.take() {
                self.retired.push(Retired { pool: Some(cap.pool), buf: Some(cap.buf) });
            }
        }
    }

    fn finish_capture(&mut self, idx: usize, qh: &QueueHandle<State>) {
        let (img, frame, cap_pool, cap_buf, output_name) = {
            let s = &mut self.sessions[idx];
            let Some(cap) = s.cap.take() else { return };
            let Some(frame) = s.frame.take() else { return };
            let img = CapturedImage {
                data: cap.map[..].to_vec(),
                format: cap.format,
                width: cap.width,
                height: cap.height,
                stride: cap.stride,
                y_invert: s.y_invert,
            };
            (img, frame, Some(cap.pool), Some(cap.buf), s.output_name)
        };
        frame.destroy();
        self.retired.push(Retired { pool: cap_pool, buf: cap_buf });

        let (Some(comp), Some(ls)) = (self.compositor.clone(), self.layer_shell.clone()) else {
            return;
        };
        let Some(output) = self.output_proxy(output_name) else { return };
        let surface = comp.create_surface(qh, ());
        let layer = ls.get_layer_surface(
            &surface,
            Some(&output),
            Layer::Overlay,
            "wl-annotate".to_string(),
            qh,
            output_name,
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        let s = &mut self.sessions[idx];
        let (cw, ch) = (img.width, img.height);
        s.img = Some(img);
        s.wl_surface = Some(surface);
        s.layer = Some(layer);
        log(self.verbose(), format_args!("captured output {output_name}: {cw}x{ch}"));
    }

    fn on_configure(&mut self, output_name: u32, width: u32, height: u32, qh: &QueueHandle<State>) {
        let Some(idx) = self.sessions.iter().position(|s| s.output_name == output_name) else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        {
            let s = &self.sessions[idx];
            if s.canvas.is_some() {
                return; // already configured
            }
        }
        let Some(img) = self.sessions[idx].img.clone() else { return };
        let scale = std::cmp::max(1, (img.width as f64 / width.max(1) as f64).round() as i32);
        let bw = width * scale as u32;
        let bh = height * scale as u32;
        let canvas = Canvas::new(&img, bw, bh);
        let Some(canvas) = canvas else {
            self.fail_session(idx, "unsupported capture pixel format");
            return;
        };

        let size = (bw * bh * 4) as usize;
        let fd = match memfd_create("wl-annotate", MFdFlags::MFD_CLOEXEC) {
            Ok(f) => f,
            Err(e) => {
                log(true, format_args!("memfd_create failed: {e}"));
                return;
            }
        };
        if let Err(e) = ftruncate(&fd, size as i64) {
            log(true, format_args!("ftruncate failed: {e}"));
            return;
        }
        let mut map = match ShmMap::new(&fd, size) {
            Ok(m) => m,
            Err(e) => {
                log(true, format_args!("mmap failed: {e}"));
                return;
            }
        };
        let Some(shm) = self.shm.clone() else { return };
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buf = pool.create_buffer(
            0,
            bw as i32,
            bh as i32,
            (bw * 4) as i32,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        canvas.write_shm_xrgb(&mut map[..], DirtyRect { x: 0, y: 0, w: bw, h: bh });

        let s = &mut self.sessions[idx];
        let Some(surface) = s.wl_surface.clone() else { return };
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buf), 0, 0);
        surface.damage_buffer(0, 0, bw as i32, bh as i32);
        surface.commit();
        s.canvas = Some(canvas);
        s.out = Some(OutBuf { map, pool, buf });
        s.scale = scale;
        s.logical = (width, height);
        s.visible = true;
        log(
            self.verbose(),
            format_args!("output {output_name}: overlay mapped at {}x{} (scale {scale})", bw, bh),
        );
    }

    fn remove_session(&mut self, output_name: u32) {
        self.sessions.retain(|s| s.output_name != output_name);
        self.pointer_session = None;
    }

    /// Flush a changed region of the canvas to the SHM buffer and commit it.
    fn redraw(&mut self, idx: usize, rect: DirtyRect) {
        let Some(s) = self.sessions.get_mut(idx) else { return };
        let (Some(canvas), Some(out), Some(surface)) =
            (&mut s.canvas, &mut s.out, &s.wl_surface)
        else {
            return;
        };
        canvas.write_shm_xrgb(&mut out.map[..], rect);
        surface.attach(Some(&out.buf), 0, 0);
        surface.damage_buffer(rect.x, rect.y, rect.w as i32, rect.h as i32);
        surface.commit();
    }

    fn pointer_motion(&mut self, idx: usize, x: f64, y: f64) {
        self.pointer_pos = Some((x as f32, y as f32));
        let pen = self.pen;
        let color = self.config.color;
        let dirty = {
            let Some(s) = self.sessions.get_mut(idx) else { return };
            if !s.drawing {
                return;
            }
            let Some(canvas) = s.canvas.as_mut() else { return };
            let (lw, lh) = (s.logical.0.max(1) as f32, s.logical.1.max(1) as f32);
            let bx = x as f32 / lw * canvas.width as f32;
            let by = y as f32 / lh * canvas.height as f32;
            let last = s.last.unwrap_or((bx, by));
            let rect = canvas.stroke_segment(last.0, last.1, bx, by, pen * s.scale as f32, color);
            s.last = Some((bx, by));
            rect
        };
        if let Some(rect) = dirty {
            self.redraw(idx, rect);
        }
    }

    fn pointer_button(&mut self, idx: usize, button: u32, pressed: bool) {
        let pen = self.pen;
        let color = self.config.color;
        match (button, pressed) {
            (BTN_LEFT, true) => {
                let Some((x, y)) = self.pointer_pos else { return };
                let dirty = {
                    let Some(s) = self.sessions.get_mut(idx) else { return };
                    s.drawing = true;
                    let Some(canvas) = s.canvas.as_mut() else { return };
                    let (lw, lh) = (s.logical.0.max(1) as f32, s.logical.1.max(1) as f32);
                    let bx = x / lw * canvas.width as f32;
                    let by = y / lh * canvas.height as f32;
                    s.last = Some((bx, by));
                    // draw a dot so a simple click leaves a mark
                    canvas.stroke_segment(bx, by, bx + 0.01, by + 0.01, pen * s.scale as f32, color)
                };
                if let Some(rect) = dirty {
                    self.redraw(idx, rect);
                }
            }
            (BTN_LEFT, false) => {
                if let Some(s) = self.sessions.get_mut(idx) {
                    s.drawing = false;
                }
            }
            (BTN_RIGHT, true) => {
                let dirty = {
                    let Some(s) = self.sessions.get_mut(idx) else { return };
                    let Some(canvas) = s.canvas.as_mut() else { return };
                    Some(canvas.clear())
                };
                if let Some(rect) = dirty {
                    self.redraw(idx, rect);
                }
            }
            _ => {}
        }
    }

    fn bind_global(&mut self, registry: &wl_registry::WlRegistry, name: u32, interface: &str, version: u32, qh: &QueueHandle<State>) {
        let v = self.verbose();
        match interface {
            "wl_compositor" => {
                if self.compositor.is_none() {
                    let end = version.min(6).min(wl_compositor::WlCompositor::interface().version).max(1);
                    self.compositor = Some(registry.bind(name, end, qh, ()));
                    log(v, format_args!("bound wl_compositor v{end}"));
                }
            }
            "wl_shm" => {
                if self.shm.is_none() {
                    let end = version.min(1).min(wl_shm::WlShm::interface().version).max(1);
                    self.shm = Some(registry.bind(name, end, qh, ()));
                }
            }
            "wl_seat" => {
                if self.seat.is_none() {
                    let end = version.min(7).min(wl_seat::WlSeat::interface().version).max(1);
                    self.seat = Some(registry.bind(name, end, qh, ()));
                }
            }
            "wl_output" => {
                let end = version.min(4).min(wl_output::WlOutput::interface().version).max(1);
                let proxy: wl_output::WlOutput = registry.bind(name, end, qh, OutputUData { name });
                log(v, format_args!("bound wl_output {name} v{end}"));
                self.outputs.push(Output { name, proxy, scale: 1, description: String::new() });
                if self.active {
                    self.start_capture(name);
                }
            }
            "zwlr_layer_shell_v1" => {
                if self.layer_shell.is_none() {
                    let max = ZwlrLayerShellV1::interface().version;
                    // prefer >= 3 (on-demand keyboard interactivity), fall back to 1..=2
                    let end = version.min(5).min(max);
                    self.layer_shell = if end >= 3 {
                        Some(registry.bind(name, end, qh, ()))
                    } else {
                        Some(registry.bind(name, end.max(1), qh, ()))
                    };
                    if let Some(ls) = &self.layer_shell {
                        log(v, format_args!("bound zwlr_layer_shell_v1 v{}", ls.version()));
                    }
                }
            }
            "zwlr_screencopy_manager_v1" => {
                if self.screencopy.is_none() {
                    let end = version.min(3).min(ZwlrScreencopyManagerV1::interface().version).max(1);
                    self.screencopy = Some(registry.bind(name, end, qh, ()));
                    if let Some(sc) = &self.screencopy {
                        log(v, format_args!("bound zwlr_screencopy_manager_v1 v{}", sc.version()));
                    }
                }
            }
            _ => {}
        }
    }

    fn remove_output(&mut self, name: u32) {
        self.outputs.retain(|o| o.name != name);
    }
}

pub fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}-{h:02}{m:02}{s:02}")
}

// ---------------------------------------------------------------------------
// Control socket
// ---------------------------------------------------------------------------

pub fn setup_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Single-instance semantics: if a previous daemon holds the control socket,
/// ask it to quit and wait for it to release takeover of the screen (two
/// live overlays would fight for keyboard focus on some compositors).
pub fn take_over_socket(path: &Path) {
    if !path.exists() {
        return;
    }
    let Ok(mut stream) = UnixStream::connect(path) else { return };
    set_rcvtimeo(&stream, 2);
    eprintln!("wl-annotate: a previous instance is running, asking it to quit");
    if stream.write_all(b"quit\n").is_ok() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply); // old daemon closes on exit
    }
    // Give the old daemon a moment to fully unmap, then fall through:
    // setup_socket() removes the stale path anyway.
    for _ in 0..200 {
        if UnixStream::connect(path).is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn set_rcvtimeo(stream: &UnixStream, secs: i64) {
    let tv = nix::libc::timeval { tv_sec: secs, tv_usec: 0 };
    unsafe {
        nix::libc::setsockopt(
            stream.as_raw_fd(),
            nix::libc::SOL_SOCKET,
            nix::libc::SO_RCVTIMEO,
            &tv as *const nix::libc::timeval as *const nix::libc::c_void,
            std::mem::size_of_val(&tv) as nix::libc::socklen_t,
        );
    }
}

pub fn accept_socket(listener: &UnixListener, state: &mut State) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                set_rcvtimeo(&stream, 1);
                let mut buf = Vec::new();
                // client writes the command and shuts down its write side;
                // the timeout protects us from clients that don't.
                let _ = stream.read_to_end(&mut buf);
                let text = String::from_utf8_lossy(&buf);
                let mut reply = String::new();
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_command(line) {
                        Some(cmd) => {
                            let r = state.handle_command(cmd);
                            reply.push_str(&r);
                            reply.push('\n');
                        }
                        None => {
                            reply.push_str(&format!("error: unknown command {line:?}\n"));
                        }
                    }
                }
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut State,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                state.bind_global(registry, name, &interface, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.remove_output(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputUData> for State {
    fn event(
        state: &mut State,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        udata: &OutputUData,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let Some(o) = state.outputs.iter_mut().find(|o| o.name == udata.name) {
            match event {
                wl_output::Event::Scale { factor } => o.scale = factor,
                wl_output::Event::Name { name } => o.description = name,
                wl_output::Event::Description { description } => o.description = description,
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut State,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if let WEnum::Value(caps) = capabilities {
                if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
                if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut State,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                if let Some(idx) = state
                    .sessions
                    .iter()
                    .position(|s| s.wl_surface.as_ref() == Some(&surface))
                {
                    state.pointer_session = Some(idx);
                    state.pointer_pos = Some((surface_x as f32, surface_y as f32));
                    state.update_cursor(Some(serial));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_session = None;
                state.pointer_pos = None;
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some(idx) = state.pointer_session {
                    state.pointer_motion(idx, surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button { button, state: btn, .. } => {
                let pressed = matches!(btn, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if let Some(idx) = state.pointer_session {
                    state.pointer_button(idx, button, pressed);
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                    state.pen = (state.pen - value as f32 / 5.0).clamp(1.0, 64.0);
                    log(true, format_args!("pen size: {:.1}", state.pen));
                    state.update_cursor(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut State,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_keyboard::Event::Enter { keys, .. } => {
                log(true, format_args!("keyboard enter (focused): {keys:?}"));
            }
            wl_keyboard::Event::Leave { .. } => {
                log(true, format_args!("keyboard leave (unfocused)"));
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                log(true, format_args!("key event: code {key}, state {ks:?}"));
                if (key_is(key, KEY_Q_EVDEV) || key_is(key, KEY_ESC_EVDEV))
                    && matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed))
                {
                    log(true, format_args!("stop drawing (q/Esc)"));
                    state.deactivate();
                    if state.config.once {
                        state.quit = true;
                    }
                } else if key_is(key, KEY_C_EVDEV)
                    && matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed))
                {
                    state.clear_all();
                } else if key_is(key, KEY_S_EVDEV)
                    && matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed))
                {
                    let r = state.save_all();
                    log(true, format_args!("{r}"));
                } else if let Some((name, color)) = color_key(key) {
                    if matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
                        state.config.color = color;
                        log(true, format_args!("pen color: {name}"));
                    }
                }
            }
            // We compare raw evdev keycodes, so the keymap fd is not needed.
            wl_keyboard::Event::Keymap { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, u32> for State {
    fn event(
        state: &mut State,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        &output_name: &u32,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        let Some(idx) = state.sessions.iter().position(|s| s.output_name == output_name) else {
            frame.destroy();
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                let Some(format) = CapturedImage::format_value(format) else {
                    state.fail_session(idx, "screencopy gave an unknown pixel format");
                    frame.destroy();
                    return;
                };
                if !matches!(
                    format,
                    wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Abgr8888
                        | wl_shm::Format::Xbgr8888
                ) {
                    state.fail_session(idx, "screencopy gave an unsupported pixel format (need 32bpp)");
                    frame.destroy();
                    return;
                }
                let size = stride as usize * height as usize;
                let fd = match memfd_create("wl-annotate", MFdFlags::MFD_CLOEXEC) {
                    Ok(f) => f,
                    Err(e) => {
                        log(true, format_args!("memfd_create failed: {e}"));
                        frame.destroy();
                        return;
                    }
                };
                if let Err(e) = ftruncate(&fd, size as i64) {
                    log(true, format_args!("ftruncate failed: {e}"));
                    frame.destroy();
                    return;
                }
                let map = match ShmMap::new(&fd, size) {
                    Ok(m) => m,
                    Err(_) => {
                        frame.destroy();
                        return;
                    }
                };
                let Some(shm) = state.shm.clone() else {
                    frame.destroy();
                    return;
                };
                let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
                let buf = pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride as i32,
                    format,
                    qh,
                    (),
                );
                frame.copy(&buf);
                state.sessions[idx].cap = Some(CaptureBuf {
                    fd,
                    map,
                    pool,
                    buf,
                    format,
                    width,
                    height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.sessions[idx].y_invert =
                        f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.finish_capture(idx, qh);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.fail_session(idx, "screencopy frame failed");
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut State,
        layer: &ZwlrLayerSurfaceV1,
        event: wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event,
        &output_name: &u32,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event;
        match event {
            Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                state.on_configure(output_name, width, height, qh);
            }
            Event::Closed => {
                state.remove_session(output_name);
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
