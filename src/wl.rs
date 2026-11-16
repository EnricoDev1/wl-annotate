//! Wayland client state: registry, screencopy freeze, layer-shell overlay,
//! pointer/keyboard drawing.

use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
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
const KEY_U_EVDEV: u32 = 22;

/// xkb modifier bit for Shift (wl_keyboard.modifiers, mods_depressed).
const MOD_SHIFT: u32 = 0x1;

/// Pen color selected by a letter key while drawing.
const RED: [u8; 4] = [0xff, 0x00, 0x00, 0xff];
const GREEN: [u8; 4] = [0x00, 0xff, 0x00, 0xff];
const BLUE: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const MAGENTA: [u8; 4] = [0xff, 0x00, 0xff, 0xff];
const WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// An in-session action bound to a physical key (evdev code).
#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyAction {
    Quit,
    Clear,
    Save,
    Undo, // u: undo, or redo when Shift is held
    Color(&'static str, [u8; 4]),
}

/// Which keycode convention the compositor delivers in wl_keyboard.key
/// events. Most compositors send the evdev code + 8 (the xkb convention);
/// niri (Smithay) delivers the raw evdev code. Calibrated at runtime:
/// the first unambiguous key press settles it, ambiguous presses
/// ("is this code a raw key, or a +8 key I might receive?") are held
/// in a tentative state and never fired, so a compositor of either kind
/// can at worst require re-pressing the first action key - never suffer
/// a phantom action or a dead "q".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyEncoding {
    /// No evidence yet.
    Unknown,
    /// One ambiguous, +8-looking key was seen. xkb compositors confirm on the
    /// next such key; a raw-looking key proves the encoding is raw instead.
    XkbTentative,
    /// Encoding settled: raw evdev codes. `xkb_hits` counts consecutive
    /// +8-looking keys seen while settled, to recover a wrong calibration.
    RawEvdev { xkb_hits: u8 },
    /// Encoding settled: evdev + 8 (xkb convention). Symmetric counter.
    XkbPlus8 { raw_hits: u8 },
}

fn action_for_evdev(code: u32) -> Option<KeyAction> {
    use KeyAction::*;
    match code {
        KEY_Q_EVDEV | KEY_ESC_EVDEV => Some(Quit),
        KEY_C_EVDEV => Some(Clear),
        KEY_S_EVDEV => Some(Save),
        KEY_U_EVDEV => Some(Undo),
        KEY_R_EVDEV => Some(Color("red", RED)),
        KEY_G_EVDEV => Some(Color("green", GREEN)),
        KEY_B_EVDEV => Some(Color("blue", BLUE)),
        KEY_M_EVDEV => Some(Color("magenta", MAGENTA)),
        KEY_W_EVDEV => Some(Color("white", WHITE)),
        _ => None,
    }
}

/// Modifier keys that are benign on a raw compositor but whose xkb reading
/// (42 -> green, 54 -> clear, 56 -> blue) is bound: they are pressed all the
/// time (shift = the redo modifier) without meaning any action, so they must
/// not be used as encoding evidence.
const BENIGN_MODIFIER_CODES: [u32; 3] = [42, 54, 56];

fn is_benign_modifier(code: u32) -> bool {
    BENIGN_MODIFIER_CODES.contains(&code)
}

/// Resolve a delivered keycode to an action, calibrating `enc` along the
/// way. Design constraints:
/// - a settled encoding must never fire the *other* convention's phantom
///   (niri user typing 'a' = raw 30 must not undo = u+8);
/// - a wrong calibration must never block the user: a raw-looking key while
///   tentative (or twice in a row while settled the other way) heals it and
///   fires;
/// - an ambiguous key never fires while unsettled, so the worst outcome is
///   re-pressing the first action key after daemon start.
///
/// Special case: delivered code KEY_M (50) is raw 'm' on niri but what an
/// xkb compositor sends for LeftShift (42+8). Shift is common (shift+u =
/// redo), so it is treated as ambiguous, not as unambiguous raw evidence.
fn resolve_action(enc: &mut KeyEncoding, code: u32) -> Option<KeyAction> {
    let raw = action_for_evdev(code);
    let xkb = code.checked_sub(8).and_then(action_for_evdev);
    match (*enc, raw, xkb) {
        // Settled: raw evdev codes (niri).
        (KeyEncoding::RawEvdev { xkb_hits }, Some(a), _) => {
            if xkb_hits != 0 {
                *enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
            }
            Some(a)
        }
        (KeyEncoding::RawEvdev { xkb_hits }, None, Some(a)) => {
            // +8-looking key on a raw compositor: phantom of the other
            // convention. Benign modifier presses never count; two real
            // action-collisions in a row mean the calibration is wrong.
            if is_benign_modifier(code) {
                return None;
            }
            if xkb_hits >= 1 {
                *enc = KeyEncoding::XkbPlus8 { raw_hits: 0 };
                log(true, format_args!("keycode encoding corrected: evdev+8 (xkb)"));
                Some(a)
            } else {
                *enc = KeyEncoding::RawEvdev { xkb_hits: 1 };
                None
            }
        }
        (KeyEncoding::RawEvdev { .. }, None, None) => None,
        // Settled: evdev + 8.
        (KeyEncoding::XkbPlus8 { raw_hits }, _, Some(a)) => {
            if raw_hits != 0 {
                *enc = KeyEncoding::XkbPlus8 { raw_hits: 0 };
            }
            Some(a)
        }
        (KeyEncoding::XkbPlus8 { raw_hits }, Some(a), None) => {
            // Raw-looking key on an xkb compositor: mirror of the above.
            if raw_hits >= 1 {
                *enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
                log(true, format_args!("keycode encoding corrected: raw evdev"));
                Some(a)
            } else {
                *enc = KeyEncoding::XkbPlus8 { raw_hits: 1 };
                None
            }
        }
        (KeyEncoding::XkbPlus8 { .. }, None, None) => None,
        // Tentative after one ambiguous key.
        (KeyEncoding::XkbTentative, None, Some(a)) => {
            if is_benign_modifier(code) {
                return None; // shifts are not evidence of an xkb compositor
            }
            *enc = KeyEncoding::XkbPlus8 { raw_hits: 0 };
            log(true, format_args!("keycode encoding calibrated: evdev+8 (xkb)"));
            Some(a)
        }
        (KeyEncoding::XkbTentative, Some(a), _) => {
            *enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
            log(true, format_args!("keycode encoding calibrated: raw evdev"));
            Some(a)
        }
        (KeyEncoding::XkbTentative, None, None) => None,
        // Unsettled.
        (KeyEncoding::Unknown, Some(a), None) if code != KEY_M_EVDEV => {
            *enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
            log(true, format_args!("keycode encoding calibrated: raw evdev"));
            Some(a)
        }
        // KEY_M delivered while unsettled: raw 'm' (niri) vs LeftShift +8
        // (xkb compositors, common for redo). Hold as tentative, no fire.
        (KeyEncoding::Unknown, Some(_), _) => {
            *enc = KeyEncoding::XkbTentative;
            None
        }
        (KeyEncoding::Unknown, None, Some(_)) => {
            *enc = KeyEncoding::XkbTentative;
            None
        }
        (KeyEncoding::Unknown, None, None) => None,
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
    /// Output whose layer surface currently has keyboard focus
    /// (target of the undo key).
    pub keyboard_session: Option<usize>,
    /// Depressed keyboard modifiers (xkb bitmask) from wl_keyboard.
    pub mods: u32,
    /// Which keycode convention this compositor delivers; calibrated from
    /// the first unambiguous session key press.
    pub key_encoding: KeyEncoding,
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
            keyboard_session: None,
            mods: 0,
            key_encoding: KeyEncoding::Unknown,
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
        self.keyboard_session = None;
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
        log(true, format_args!("cleared"));
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
        self.keyboard_session = None;
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
                    // one drag (press -> release) is one undo step
                    canvas.begin_step();
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
                    if let Some(canvas) = s.canvas.as_mut() {
                        canvas.end_step(); // commit the drag to the undo history
                    }
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

/// Outcome of a probe against the control socket path.
enum Probe {
    /// A listener accepted (or queued) us: an instance is alive.
    Connected(UnixStream),
    /// No listener: stale socket file or the previous daemon exited.
    Refused,
    /// Listener exists but refuses to queue us right now (hung + full
    /// backlog): treat as alive.
    Busy,
}

/// Connect without ever sleeping in the kernel: a blocking client socket can
/// hang forever in unix_wait_for_peer() against a hung foreign listener that
/// never accept()s, and the takeover must be deadlock-free.
fn probe_socket(path: &Path) -> Probe {
    let Ok(addr) = nix::sys::socket::UnixAddr::new(path) else {
        return Probe::Refused;
    };
    let Ok(fd) = nix::sys::socket::socket(
        nix::sys::socket::AddressFamily::Unix,
        nix::sys::socket::SockType::Stream,
        nix::sys::socket::SockFlag::SOCK_NONBLOCK,
        None,
    ) else {
        return Probe::Busy;
    };
    match nix::sys::socket::connect(fd.as_raw_fd(), &addr) {
        Ok(()) => {
            let stream = unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) };            // back to blocking for the polite-quit conversation
            let _ = stream.set_nonblocking(false);
            Probe::Connected(stream)
        }
        Err(nix::errno::Errno::ECONNREFUSED)
        | Err(nix::errno::Errno::ENOENT)
        | Err(nix::errno::Errno::ECONNRESET)
        | Err(nix::errno::Errno::EPIPE) => Probe::Refused,
        Err(_) => Probe::Busy,
    }
}

fn wait_socket_free(path: &Path) -> bool {
    for _ in 0..200 {
        if matches!(probe_socket(path), Probe::Refused) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Single-instance semantics: if a previous daemon holds the control socket,
/// ask it to quit and wait for it to release takeover of the screen (two
/// live overlays would fight for keyboard focus on some compositors).
///
/// If the previous daemon is hung and ignores the polite quit, identify the
/// holding PID via SO_PEERCRED and SIGKILL it (guarded by a /proc cmdline
/// check so a recycled PID is never killed).
pub fn take_over_socket(path: &Path) {
    if !path.exists() {
        return;
    }
    let mut stream = match probe_socket(path) {
        Probe::Refused => return, // stale socket file, no listener
        Probe::Busy => {
            // could not even queue us; it may be exiting, give it a moment
            if wait_socket_free(path) {
                return;
            }
            eprintln!("wl-annotate: previous instance is busy, forcing takeover");
            kill_stale_holder(path);
            let _ = wait_socket_free(path);
            return;
        }
        Probe::Connected(stream) => stream,
    };
    set_rcvtimeo(&stream, 2);
    eprintln!("wl-annotate: a previous instance is running, asking it to quit");
    if stream.write_all(b"quit\n").is_ok() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply); // old daemon closes on exit
    }
    // The old daemon closes the socket on exit. Polling also reconnects to
    // it: the pending connection is the event that lets a busy event loop
    // observe Command::Quit and exit.
    if wait_socket_free(path) {
        return; // previous instance exited cleanly
    }
    // Still listening after 2s: the previous daemon is hung.
    eprintln!("wl-annotate: previous instance is not responding, forcing takeover");
    kill_stale_holder(path);
    let _ = wait_socket_free(path);
}

/// SIGKILL the process holding a hung control socket, after verifying via
/// /proc that the PID still belongs to us (guards against PID reuse).
/// SIGKILL the process holding a hung control socket. Guarded: only a
/// process whose argv[0] basename is `wl-annotate` is ever killed —
/// never a recycled PID or a foreign process that happens to mention the
/// socket path in its arguments.
fn kill_stale_holder(path: &Path) {
    let stream = match probe_socket(path) {
        Probe::Connected(stream) => stream,
        _ => {
            eprintln!(
                "wl-annotate: socket {} is still occupied but the holder is not identifiable; leaving it alone",
                path.display()
            );
            return;
        }
    };
    let Ok(cred) =
        nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
    else {
        return;
    };
    let pid = cred.pid();
    if pid <= 0 {
        return;
    }
    if !holder_is_wl_annotate(pid) {
        eprintln!(
            "wl-annotate: socket {} is held by pid {pid}, which is not wl-annotate; leaving it alone",
            path.display()
        );
        return;
    }
    eprintln!("wl-annotate: killing hung instance pid {pid}");
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::SIGKILL,
    );
}

/// True if /proc/`pid` was started as a `wl-annotate` binary (argv[0]
/// basename check: robust against PID reuse and against processes that
/// merely mention the socket path in their arguments).
fn holder_is_wl_annotate(pid: i32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    match std::str::from_utf8(argv0).ok().map(Path::new) {
        Some(p) => p.file_name().and_then(|s| s.to_str()) == Some("wl-annotate"),
        None => false,
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
            wl_keyboard::Event::Enter { keys, surface, .. } => {
                let idx = state
                    .sessions
                    .iter()
                    .position(|s| s.wl_surface.as_ref() == Some(&surface));
                if let Some(i) = idx {
                    state.keyboard_session = Some(i);
                }
                log(true, format_args!("keyboard enter (focused): {keys:?}"));
            }
            wl_keyboard::Event::Leave { .. } => {
                state.keyboard_session = None;
                state.mods = 0;
                log(true, format_args!("keyboard leave (unfocused)"));
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                log(true, format_args!("key event: code {key}, state {ks:?}"));
                if !matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
                    return;
                }
                let action = resolve_action(&mut state.key_encoding, key);
                let Some(action) = action else { return };
                match action {
                    KeyAction::Quit => {
                        log(true, format_args!("stop drawing (q/Esc)"));
                        state.deactivate();
                        if state.config.once {
                            state.quit = true;
                        }
                    }
                    KeyAction::Clear => state.clear_all(),
                    KeyAction::Save => {
                        let r = state.save_all();
                        log(true, format_args!("{r}"));
                    }
                    KeyAction::Color(name, color) => {
                        state.config.color = color;
                        log(true, format_args!("pen color: {name}"));
                    }
                    KeyAction::Undo => {
                        let target = state.keyboard_session.or(state.pointer_session);
                        let undo = state.mods & MOD_SHIFT == 0;
                        let dirty = target.and_then(|i| {
                            let canvas = state.sessions.get_mut(i)?.canvas.as_mut()?;
                            if undo {
                                canvas.undo()
                            } else {
                                canvas.redo()
                            }
                        });
                        let what = if undo { "undo" } else { "redo" };
                        match (target, dirty) {
                            (Some(i), Some(rect)) => {
                                state.redraw(i, rect);
                                log(true, format_args!("{what}"));
                            }
                            (_, None) => log(true, format_args!("nothing to {what}")),
                            (None, Some(_)) => {}
                        }
                    }
                }
            }
            // We compare raw evdev keycodes, so the keymap fd is not needed.
            wl_keyboard::Event::Keymap { .. } => {}
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                state.mods = mods_depressed;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_comp_first_action_key_fires_immediately() {
        let mut enc = KeyEncoding::Unknown;
        // q = raw 16 (niri), unambiguous: calibrate raw and fire now.
        assert_eq!(resolve_action(&mut enc, KEY_Q_EVDEV), Some(KeyAction::Quit));
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 });
    }

    #[test]
    fn xkb_comp_needs_two_presses_then_confirms() {
        let mut enc = KeyEncoding::Unknown;
        // On an xkb compositor, 'u' is delivered as evdev+8 = 30, which raw-reads
        // as 'a': ambiguous. First press must be silent, not a phantom undo.
        assert_eq!(resolve_action(&mut enc, KEY_U_EVDEV + 8), None);
        assert_eq!(enc, KeyEncoding::XkbTentative);
        // The next xkb-looking key confirms xkb and fires.
        assert_eq!(resolve_action(&mut enc, KEY_U_EVDEV + 8), Some(KeyAction::Undo));
        assert_eq!(enc, KeyEncoding::XkbPlus8 { raw_hits: 0 });
    }

    #[test]
    fn ambiguous_key_then_raw_key_heals() {
        let mut enc = KeyEncoding::Unknown;
        // niri user presses 'a' (30): xkb-looking, silent tentative.
        assert_eq!(resolve_action(&mut enc, 30), None);
        assert_eq!(enc, KeyEncoding::XkbTentative);
        // Then presses q (16): raw-looking; heals to raw and fires. This is
        // the flow that must never leave the user stuck.
        assert_eq!(resolve_action(&mut enc, KEY_Q_EVDEV), Some(KeyAction::Quit));
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 });
    }

    #[test]
    fn raw_comp_phantoms_are_rejected() {
        let mut enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
        // 'a' = raw 30 = u+8: phantom undo must be rejected on niri.
        assert_eq!(resolve_action(&mut enc, 30), None);
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 1 });
        // Another +8-looking key would heal a wrong raw calibration; the
        // single hit above must not be reset by it. Now heal with u+8.
        assert_eq!(resolve_action(&mut enc, KEY_U_EVDEV + 8), Some(KeyAction::Undo));
        assert_eq!(enc, KeyEncoding::XkbPlus8 { raw_hits: 0 });
    }

    #[test]
    fn xkb_comp_phantoms_are_rejected() {
        let mut enc = KeyEncoding::XkbPlus8 { raw_hits: 0 };
        // KEY_C raw-delivered reads as 'l' on an xkb compositor; the raw
        // Clear hypothesis must be rejected.
        assert_eq!(resolve_action(&mut enc, KEY_C_EVDEV), None);
        assert_eq!(enc, KeyEncoding::XkbPlus8 { raw_hits: 1 });
        // Second raw-looking key flips to raw and fires (mash to recover).
        assert_eq!(resolve_action(&mut enc, KEY_Q_EVDEV), Some(KeyAction::Quit));
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 });
    }

    #[test]
    fn m_key_is_ambiguous_while_unsettled() {
        // Delivered 50 = raw 'm' (niri) vs LeftShift on xkb compositors
        // (42+8). Shift is the redo modifier, so a first press must not
        // flash magenta and mis-calibrate.
        let mut enc = KeyEncoding::Unknown;
        assert_eq!(resolve_action(&mut enc, KEY_M_EVDEV), None);
        assert_eq!(enc, KeyEncoding::XkbTentative);
        // Repeating 'm' resolves it as raw magenta.
        let fired = resolve_action(&mut enc, KEY_M_EVDEV);
        assert!(matches!(fired, Some(KeyAction::Color("magenta", _))));
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 });
    }

    #[test]
    fn actions_fire_after_raw_calibration() {
        let mut enc = KeyEncoding::Unknown;
        let color = resolve_action(&mut enc, KEY_G_EVDEV);
        assert!(matches!(color, Some(KeyAction::Color("green", _))));
        assert_eq!(resolve_action(&mut enc, KEY_C_EVDEV), Some(KeyAction::Clear));
        assert_eq!(resolve_action(&mut enc, KEY_U_EVDEV), Some(KeyAction::Undo));
    }

    #[test]
    fn shift_presses_are_not_encoding_evidence() {
        // Shift is the redo modifier and is pressed all the time with no
        // action intent. It must neither confirm a tentative xkb encoding
        // nor accumulate collision hits that could let an unrelated 'a'
        // flip the calibration on niri.
        let mut enc = KeyEncoding::Unknown;
        assert_eq!(resolve_action(&mut enc, 30), None); // 'a': tentative
        assert_eq!(
            resolve_action(&mut enc, 42), // LeftShift
            None
        );
        assert_eq!(enc, KeyEncoding::XkbTentative, "shift must not confirm tentative");
        assert_eq!(
            resolve_action(&mut enc, 30), // 'a' again: confirm is fine here
            Some(KeyAction::Undo)
        );

        let mut enc = KeyEncoding::RawEvdev { xkb_hits: 0 };
        assert_eq!(resolve_action(&mut enc, 42), None); // LeftShift
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 }, "no hit from shift");
        assert_eq!(resolve_action(&mut enc, 54), None); // RightShift
        assert_eq!(resolve_action(&mut enc, 56), None); // LeftAlt
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 0 });
        // A benign modifier in between must not reset or double the counter:
        // one real collision stays pending, and the second real collision
        // heals. Shift alone never advances it.
        assert_eq!(resolve_action(&mut enc, 30), None); // 'a': hit 1
        assert_eq!(resolve_action(&mut enc, 42), None); // shift in between
        assert_eq!(enc, KeyEncoding::RawEvdev { xkb_hits: 1 });
        assert_eq!(resolve_action(&mut enc, 30), Some(KeyAction::Undo)); // 'a': heal
        assert_eq!(enc, KeyEncoding::XkbPlus8 { raw_hits: 0 });
    }
}
