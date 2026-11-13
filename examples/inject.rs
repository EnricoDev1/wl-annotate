//! Dev tool: inject a key through zwp_virtual_keyboard_v1 (dev-dependency only).
//!
//! Usage: cargo run --release --example inject [keycode] [hold_ms]
//! Defaults: injects raw KEY_Q (16), holds 80ms.
//!
//! The keycode is sent as given, unchanged. Compositors differ in what they
//! forward to the focused client: niri 26.04 delivers raw evdev codes, while
//! most compositors add +8 (xkb convention). niri forwards the injected value
//! as-is, so inject 16 to reproduce a physical "q" on niri.

use std::collections::HashMap;
use std::os::fd::AsFd;

use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::unistd::ftruncate;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const KEYMAP: &str = "\
xkb_keymap {
    xkb_keycodes { include \"evdev\" };
    xkb_types { include \"complete\" };
    xkb_compat { include \"complete\" };
    xkb_symbols { include \"pc+us\" };
};
\0";

struct St {
    globals: HashMap<String, (u32, u32)>, // interface -> (name, version)
    seat: Option<wl_seat::WlSeat>,
    vkm: Option<ZwpVirtualKeyboardManagerV1>,
}

impl St {
    fn new() -> St {
        St { globals: HashMap::new(), seat: None, vkm: None }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for St {
    fn event(
        state: &mut St,
        registry: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<St>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            state.globals.insert(interface.clone(), (name, version));
            if interface == "wl_seat" {
                state.seat = Some(registry.bind(name, version.min(9), qh, ()));
            }
            if interface == "zwp_virtual_keyboard_manager_v1" {
                state.vkm = Some(registry.bind(name, version, qh, ()));
            }
        }
    }
}

fn main() {    let send_code: u32 = std::env::args().nth(1).map(|a| a.parse().expect("code")).unwrap_or(16);
    let hold_ms: u64 = std::env::args().nth(2).map(|a| a.parse().expect("ms")).unwrap_or(80);

    let conn = Connection::connect_to_env().expect("connect");
    let mut queue = conn.new_event_queue::<St>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut st = St::new();
    queue.roundtrip(&mut st).expect("roundtrip");

    let seat = st.seat.clone().expect("no wl_seat global");
    let vkm = st.vkm.clone().expect("no zwp_virtual_keyboard_manager_v1 global");
    let vk = vkm.create_virtual_keyboard(&seat, &qh, ());

    // send the keymap first (required before key events)
    let bytes = KEYMAP.as_bytes();
    let fd = memfd_create("wl-annotate-inject", MFdFlags::MFD_CLOEXEC).expect("memfd");
    ftruncate(&fd, bytes.len() as i64).expect("ftruncate");
    let map = unsafe {
        nix::sys::mman::mmap(
            None,
            std::num::NonZeroUsize::new(bytes.len()).unwrap(),
            nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
            nix::sys::mman::MapFlags::MAP_SHARED,
            &fd,
            0,
        )
    }
    .expect("mmap");
    unsafe {
        std::slice::from_raw_parts_mut(map.as_ptr().cast::<u8>(), bytes.len())
            .copy_from_slice(bytes);
    }
    // format 1 = XKB_KEYMAP_FORMAT_TEXT_V1
    vk.keymap(1, fd.as_fd(), bytes.len() as u32);
    conn.flush().expect("flush keymap");
    queue.roundtrip(&mut st).expect("roundtrip keymap");

    std::thread::sleep(std::time::Duration::from_millis(150));

    eprintln!("injecting key: code={send_code}");
    vk.key(0, send_code, 1); // pressed
    conn.flush().expect("flush press");
    queue.roundtrip(&mut st).expect("roundtrip press");
    std::thread::sleep(std::time::Duration::from_millis(hold_ms));
    vk.key(hold_ms as u32, send_code, 0); // released
    conn.flush().expect("flush release");
    queue.roundtrip(&mut st).expect("roundtrip release");
    eprintln!("done");
}

wayland_client::delegate_noop!(St: ignore ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(St: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(St: ignore wl_seat::WlSeat);
