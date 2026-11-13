//! Configuration: simple `key = value` file plus CLI overrides.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    /// Pen color, RGBA.
    pub color: [u8; 4],
    /// Pen width in logical pixels.
    pub pen_size: f32,
    /// Unix socket path for control commands (wl-annotatectl).
    pub socket_path: PathBuf,
    /// Directory where "save" writes PNG snapshots.
    pub save_dir: PathBuf,
    /// Activate immediately and quit when the session ends.
    pub once: bool,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            color: [0xff, 0x00, 0x00, 0xff],
            pen_size: 4.0,
            socket_path: default_socket(),
            save_dir: default_save_dir(),
            once: false,
            verbose: false,
        }
    }
}

pub fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("wl-annotate.sock"),
        None => PathBuf::from("/tmp/wl-annotate.sock"),
    }
}

pub fn default_save_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("Pictures");
        if p.is_dir() {
            return p;
        }
    }
    PathBuf::from(".")
}

/// Parse `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Result<[u8; 4], String> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    let hex = |c: char| -> Result<u8, String> {
        c.to_digit(16).map(|d| d as u8).ok_or_else(|| format!("bad hex digit {c:?}"))
    };
    match s.len() {
        3 => Ok([hex(s.as_bytes()[0] as char)? * 17, hex(s.as_bytes()[1] as char)? * 17, hex(s.as_bytes()[2] as char)? * 17, 255]),
        6 => Ok([
            u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?,
            u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?,
            u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?,
            255,
        ]),
        8 => Ok([
            u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?,
            u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?,
            u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?,
            u8::from_str_radix(&s[6..8], 16).map_err(|e| e.to_string())?,
        ]),
        _ => Err(format!("color must be #rgb, #rrggbb or #rrggbbaa, got {s:?}")),
    }
}

/// Load config from a file; missing file is fine (defaults are used).
pub fn load(path: &Path) -> Result<Config, String> {
    let mut cfg = Config::default();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cfg),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{}:{}: expected `key = value`", path.display(), lineno + 1));
        };
        apply_kv(&mut cfg, key.trim(), value.trim())
            .map_err(|e| format!("{}:{}: {e}", path.display(), lineno + 1))?;
    }
    Ok(cfg)
}

pub fn apply_kv(cfg: &mut Config, key: &str, value: &str) -> Result<(), String> {
    let value = value.trim_matches('"').trim_matches('\'');
    match key {
        "color" => cfg.color = parse_color(value)?,
        "pen_size" | "pen" => {
            cfg.pen_size = value.parse::<f32>().map_err(|e| e.to_string())?;
            if cfg.pen_size <= 0.0 {
                return Err("pen_size must be > 0".into());
            }
        }
        "socket" => cfg.socket_path = PathBuf::from(expand_tilde(value)),
        "save_dir" => cfg.save_dir = PathBuf::from(expand_tilde(value)),
        "once" => cfg.once = parse_bool(value)?,
        "verbose" => cfg.verbose = parse_bool(value)?,
        other => eprintln!("wl-annotate: warning: unknown config key {other:?}"),
    }
    Ok(())
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(format!("expected true/false, got {v:?}")),
    }
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff").unwrap(), [255, 255, 255, 255]);
        assert_eq!(parse_color("#ff3b30").unwrap(), [255, 59, 48, 255]);
        assert_eq!(parse_color("#ff3b3080").unwrap(), [255, 59, 48, 128]);
        assert!(parse_color("#12").is_err());
        assert!(parse_color("#zzzzzz").is_err());
    }

    #[test]
    fn kv() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "color", "\"#00ff00\"").unwrap();
        assert_eq!(cfg.color, [0, 255, 0, 255]);
        apply_kv(&mut cfg, "pen_size", "8").unwrap();
        assert_eq!(cfg.pen_size, 8.0);
        assert!(apply_kv(&mut cfg, "pen_size", "-3").is_err());
    }

    #[test]
    fn file_parse() {
        let dir = std::env::temp_dir().join(format!("wl-annotate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config");
        std::fs::write(&f, "# comment\ncolor = #123456\npen_size = 6\n\n").unwrap();
        let cfg = load(&f).unwrap();
        assert_eq!(cfg.color, [0x12, 0x34, 0x56, 255]);
        assert_eq!(cfg.pen_size, 6.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
