use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

struct St;

impl Dispatch<wl_registry::WlRegistry, ()> for St {
    fn event(
        _: &mut St,
        _: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<St>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            println!("{name}\t{interface}\t{version}");
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut queue = conn.new_event_queue::<St>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut st = St;
    queue.roundtrip(&mut st).unwrap();
}
