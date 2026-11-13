//! wl-annotate — draw annotations directly on the screen on Wayland.
//!
//! On activation it freezes the screen (zwlr-screencopy snapshot), maps a
//! layer-shell `overlay` surface on every output, and lets you draw on it with
//! the pointer. Toggle with `wl-annotatectl toggle` (bind that to a compositor
//! key) or SIGUSR1.

mod config;
mod render;
mod wl;

use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode};
use wayland_client::Connection;

use config::Config;
use wl::{Command, State};

struct Args {
    config_path: Option<PathBuf>,
    once: bool,
    color: Option<String>,
    pen: Option<f32>,
    socket: Option<PathBuf>,
    save_dir: Option<PathBuf>,
    verbose: bool,
    help: bool,
}

const HELP: &str = "\
wl-annotate — screen annotation overlay for Wayland

USAGE:
    wl-annotate [OPTIONS]

OPTIONS:
    -c, --config FILE     Config file (default: $XDG_CONFIG_HOME/wl-annotate/config)
        --once            Start drawing immediately, exit when done
        --color COLOR     Pen color #rgb/#rrggbb/#rrggbbaa
        --pen N           Pen width in logical pixels
        --socket PATH     Control socket (default: $XDG_RUNTIME_DIR/wl-annotate.sock)
        --save-dir DIR    Where 'save' writes PNGs (default: ~/Pictures)
    -v, --verbose         Verbose logging to stderr
    -h, --help            Show this help

CONTROL (while running):
    wl-annotatectl toggle|on|off|clear|save|status|quit
    kill -USR1 <pid>      toggle drawing
    kill -USR2 <pid>      clear annotations

IN-SESSION KEYS (while drawing):
    q (or Esc)            stop drawing (restores the screen)
    c                     clear annotations
    s                     save PNG snapshot(s)
    r b g m w             pen color: red, blue, green, magenta, white
    mouse wheel           change pen size (the cursor ring follows)
    left drag             draw    right click: clear
";

fn parse_args() -> Args {
    let mut args = Args {
        config_path: None,
        once: false,
        color: None,
        pen: None,
        socket: None,
        save_dir: None,
        verbose: false,
        help: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--config" => args.config_path = it.next().map(PathBuf::from),
            "--once" => args.once = true,
            "--color" => args.color = it.next(),
            "--pen" => args.pen = it.next().and_then(|v| v.parse().ok()),
            "--socket" => args.socket = it.next().map(PathBuf::from),
            "--save-dir" => args.save_dir = it.next().map(PathBuf::from),
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => args.help = true,
            other => {
                eprintln!("wl-annotate: unknown argument {other:?}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("wl-annotate").join("config"))
}

fn main() {
    let args = parse_args();
    if args.help {
        print!("{HELP}");
        return;
    }

    let cfg_path = args.config_path.clone().or_else(default_config_path);
    let mut cfg = match cfg_path.as_deref() {
        Some(p) => config::load(p).unwrap_or_else(|e| {
            eprintln!("wl-annotate: {e}");
            std::process::exit(2);
        }),
        None => Config::default(),
    };
    if args.once {
        cfg.once = true;
    }
    if args.verbose {
        cfg.verbose = true;
    }
    if let Some(c) = &args.color {
        cfg.color = config::parse_color(c).unwrap_or_else(|e| {
            eprintln!("wl-annotate: bad --color: {e}");
            std::process::exit(2);
        });
    }
    if let Some(p) = args.pen {
        cfg.pen_size = p;
    }
    if let Some(s) = args.socket {
        cfg.socket_path = s;
    }
    if let Some(d) = args.save_dir {
        cfg.save_dir = d;
    }

    let conn = Connection::connect_to_env().unwrap_or_else(|e| {
        eprintln!("wl-annotate: cannot connect to Wayland display: {e}");
        eprintln!("(is WAYLAND_DISPLAY set and a compositor running?)");
        std::process::exit(1);
    });
    // Own registry on the state queue: global events are dispatched straight
    // into State::bind_global during the roundtrips below (registry_queue_init
    // would swallow them into an internal GlobalList we'd never see).
    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let (cmd_tx, cmd_rx) = calloop::channel::channel::<Command>();
    let mut state = State::new(cfg, qh.clone());

    // initial roundtrip to bind all globals
    if let Err(e) = event_queue.roundtrip(&mut state) {
        eprintln!("wl-annotate: initial roundtrip failed: {e}");
        std::process::exit(1);
    }
    if state.compositor.is_none() || state.shm.is_none() || state.seat.is_none() {
        eprintln!("wl-annotate: compositor lacks wl_compositor/wl_shm/wl_seat");
        std::process::exit(1);
    }

    let mut event_loop: EventLoop<State> = EventLoop::try_new().expect("calloop init");
    let handle = event_loop.handle();

    handle
        .insert_source(cmd_rx, |event, _, state| {
            if let calloop::channel::Event::Msg(cmd) = event {
                let _reply = state.handle_command(cmd);
            }
        })
        .expect("insert channel");

    calloop_wayland_source::WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .expect("insert wayland source");

    // control socket (take over from any previous daemon instance first)
    wl::take_over_socket(&state.config.socket_path);
    let listener = wl::setup_socket(&state.config.socket_path).unwrap_or_else(|e| {
        eprintln!("wl-annotate: cannot bind control socket {}: {e}", state.config.socket_path.display());
        std::process::exit(1);
    });
    wl::log(state.config.verbose, format_args!("control socket: {}", state.config.socket_path.display()));
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state| {
                wl::accept_socket(listener, state);
                Ok(calloop::PostAction::Continue)
            },
        )
        .expect("insert socket");

    // signals: USR1 = toggle, USR2 = clear, INT/TERM = quit
    {
        use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
        let tx = cmd_tx.clone();
        std::thread::spawn(move || {
            let mut signals =
                signal_hook::iterator::Signals::new([SIGUSR1, SIGUSR2, SIGINT, SIGTERM])
                    .expect("signals");
            for sig in signals.forever() {
                let cmd = match sig {
                    SIGUSR1 => Command::Toggle,
                    SIGUSR2 => Command::Clear,
                    _ => Command::Quit,
                };
                let _ = tx.send(cmd);
            }
        });
    }

    if state.config.once {
        let _ = cmd_tx.send(Command::On);
    }

    wl::log(
        state.config.verbose,
        format_args!(
            "ready: {} output(s), layer-shell {}, screencopy {}",
            state.outputs.len(),
            if state.layer_shell.is_some() { "ok" } else { "MISSING" },
            if state.screencopy.is_some() { "ok" } else { "MISSING" },
        ),
    );

    loop {
        if let Err(e) = event_loop.dispatch(None, &mut state) {
            eprintln!("wl-annotate: event loop error: {e}");
            break;
        }
        if state.quit {
            break;
        }
    }
    state.deactivate();
}
