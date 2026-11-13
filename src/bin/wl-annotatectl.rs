//! wl-annotatectl — send a control command to a running wl-annotate daemon.
//!
//! Usage: wl-annotatectl [--socket PATH] [toggle|on|off|clear|save|status|quit]
//!
//! Bind this to a key in your compositor, e.g. in niri:
//!   Mod+D { spawn "wl-annotatectl" "toggle"; }

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

const HELP: &str = "\
wl-annotatectl — control a running wl-annotate daemon

USAGE:
    wl-annotatectl [--socket PATH] [COMMAND]

COMMANDS:
    toggle   start/stop drawing (default)
    on       start drawing        off   stop drawing
    clear    erase annotations    save  save PNG snapshot(s)
    status   print daemon state   quit  stop the daemon
";

fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("wl-annotate.sock"),
        None => PathBuf::from("/tmp/wl-annotate.sock"),
    }
}

fn main() {
    let mut socket = default_socket();
    let mut cmd = "toggle".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = it.next() {
                    socket = PathBuf::from(s);
                }
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            other => cmd = other.to_string(),
        }
    }

    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wl-annotatectl: cannot connect to {}: {e}", socket.display());
            eprintln!("(is wl-annotate running?)");
            std::process::exit(1);
        }
    };
    if stream.write_all(cmd.as_bytes()).and_then(|_| stream.write_all(b"\n")).is_err() {
        eprintln!("wl-annotatectl: write failed");
        std::process::exit(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    if stream.read_to_string(&mut reply).is_err() {
        eprintln!("wl-annotatectl: read failed");
        std::process::exit(1);
    }
    print!("{reply}");
    if reply.starts_with("error") {
        std::process::exit(1);
    }
}
