//! Dev tool: inject pointer input through zwlr_virtual_pointer_v1.
//!
//! Usage: cargo run --release --example pointer-inject
//!
//! Warps the pointer to the middle of the screen (triggering Enter on the
//! wl-annotate overlay), presses BTN_LEFT, drags a short stroke, releases,
//! then scrolls the wheel once (pen size change).

use std::os::unix::net::UnixStream;
use std::time::{SystemTime, UNIX_EPOCH};

use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

const BTN_LEFT: u32 = 0x110;

struct St {
    seat: Option<wl_seat::WlSeat>,
    vpm: Option<ZwlrVirtualPointerManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for St {
    fn event(
        state: &mut St,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<St>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            if interface == "wl_seat" {
                state.seat = Some(reg.bind(name, version.min(8), qh, ()));
            } else if interface == "zwlr_virtual_pointer_manager_v1" {
                state.vpm = Some(reg.bind(name, version.min(2), qh, ()));
            }
        }
    }
}

fn now_ms() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u32
}

fn wait_daemon_active() -> bool {
    let sock = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/wl-annotate.sock"))
        .unwrap_or("/tmp/wl-annotate.sock".into());
    for _ in 0..40 {
        if let Ok(mut s) = UnixStream::connect(&sock) {
            use std::io::Write;
            let _ = s.write_all(b"status\n");
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut buf);
            if buf.contains("active") {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

fn main() {
    if !wait_daemon_active() {
        eprintln!("wl-annotate not active or socket unreachable, aborting");
        std::process::exit(1);
    }

    let conn = Connection::connect_to_env().expect("connect");
    let mut queue = conn.new_event_queue::<St>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut st = St { seat: None, vpm: None };
    queue.roundtrip(&mut st).expect("roundtrip");

    let seat = st.seat.clone().expect("no wl_seat");
    let vpm = st.vpm.clone().expect("no zwlr_virtual_pointer_manager_v1 global");
    let vp = vpm.create_virtual_pointer(Some(&seat), &qh, ());

    let step = |vp: &ZwlrVirtualPointerV1, conn: &Connection, queue: &mut wayland_client::EventQueue<St>, st: &mut St| {
        conn.flush().expect("flush");
        queue.roundtrip(st).expect("roundtrip");
        std::thread::sleep(std::time::Duration::from_millis(40));
    };

    // warp to the middle: Enter on the overlay -> cursor surface gets installed
    vp.motion_absolute(now_ms(), 960, 600, 1920, 1200);
    vp.frame();
    step(&vp, &conn, &mut queue, &mut st);

    // press, stroke, release
    vp.button(now_ms(), BTN_LEFT, wl_pointer::ButtonState::Pressed);
    vp.frame();
    step(&vp, &conn, &mut queue, &mut st);
    for (x, y) in [(1000, 640), (1100, 700), (1400, 500)] {
        vp.motion_absolute(now_ms(), x, y, 1920, 1200);
        vp.frame();
        step(&vp, &conn, &mut queue, &mut st);
    }
    vp.button(now_ms(), BTN_LEFT, wl_pointer::ButtonState::Released);
    vp.frame();
    step(&vp, &conn, &mut queue, &mut st);

    // wheel scroll: one notch to change pen size -> cursor should rebuild
    vp.axis(now_ms(), wl_pointer::Axis::VerticalScroll, 120.0);
    vp.frame();
    step(&vp, &conn, &mut queue, &mut st);

    eprintln!("pointer injected: warp + stroke + scroll");
}

wayland_client::delegate_noop!(St: ignore ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(St: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(St: ignore wl_seat::WlSeat);
