//! Shared helpers for OsmiumOS tools. Std only: no crates to fetch at build time.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub type Result<T = ()> = std::result::Result<T, String>;

pub const REPO_URL: &str = "https://github.com/EtliBiftek/osmium-os/releases/download/repo";

/// XDG base dir: `$<var>` if set and absolute, else `$HOME/<fallback>`.
pub fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Ok(p);
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback))
}

pub fn config_home() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn data_home() -> Result<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

pub fn state_home() -> Result<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// Prints the error and exits 1. For `main`.
pub fn exit_on_err(tool: &str, res: Result) {
    if let Err(e) = res {
        eprintln!("{tool}: {e}");
        std::process::exit(1);
    }
}

/// Runs a command and fails if it exits non-zero.
pub fn run(cmd: &mut Command) -> Result {
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed: {status}"))
    }
}

/// Runs a command and returns its stdout.
pub fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        String::from_utf8(out.stdout).map_err(|e| format!("{cmd:?}: {e}"))
    } else {
        Err(format!(
            "{cmd:?} failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// True if the command runs and exits zero. Output is discarded.
pub fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Removes a file or symlink; missing is fine.
pub fn remove_file(path: impl AsRef<Path>) -> Result {
    ignore_missing(fs::remove_file(&path), path.as_ref())
}

/// Removes a directory tree; missing is fine.
pub fn remove_dir(path: impl AsRef<Path>) -> Result {
    ignore_missing(fs::remove_dir_all(&path), path.as_ref())
}

fn ignore_missing(res: io::Result<()>, path: &Path) -> Result {
    match res {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// Appends text to a file, creating it if needed.
pub fn append(path: impl AsRef<Path>, text: &str) -> Result {
    use io::Write;
    let path = path.as_ref();
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// JSON string literal for `s`.
pub fn json_str(s: &str) -> String {
    use std::fmt::Write;
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Scalar fields of each object in a top-level JSON array, as text. Nested
/// values are skipped: enough for `hyprctl monitors -j`, and there is no JSON crate.
pub fn json_objects(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut out: Vec<BTreeMap<String, String>> = Vec::new();
    let mut depth = 0;
    let mut key: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let value = match c {
            '{' | '[' => {
                depth += 1;
                key = None;
                if c == '{' && depth == 2 {
                    out.push(BTreeMap::new());
                }
                continue;
            }
            '}' | ']' => {
                depth -= 1;
                continue;
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('u') => {
                                let hex: String = chars.by_ref().take(4).collect();
                                s.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                            }
                            Some(c) => s.push(c),
                            None => {}
                        },
                        c => s.push(c),
                    }
                }
                s
            }
            c if c.is_ascii_alphanumeric() || c == '-' => {
                let mut s = String::from(c);
                while let Some(n) = chars.next_if(|n| !matches!(n, ',' | '}' | ']') && !n.is_whitespace()) {
                    s.push(n);
                }
                s
            }
            _ => continue,
        };
        if depth == 2 {
            match (key.take(), out.last_mut()) {
                (Some(k), Some(object)) => {
                    object.insert(k, value);
                }
                _ => key = Some(value),
            }
        }
    }
    out
}

/// `hyprctl`, pointed at the newest compositor instance when the environment names none:
/// user units start outside the compositor's environment.
pub fn hyprctl() -> Command {
    let mut cmd = Command::new("hyprctl");
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
        && let Some(sig) = newest_hypr_instance()
    {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
    }
    cmd
}

fn newest_hypr_instance() -> Option<std::ffi::OsString> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.file_name())
}

/// Installed by osmium-keyring; its presence means the repo is signed.
pub const KEYRING: &str = "/usr/share/pacman/keyrings/osmium.gpg";

/// pacman.conf section for the OsmiumOS repo.
pub fn osmium_repo_section(server: &str, signed: bool) -> String {
    let siglevel = if signed {
        "SigLevel = Required DatabaseRequired"
    } else {
        "# Unsigned until a signing key is set up (see README, Signing).\nSigLevel = Optional TrustAll"
    };
    format!("\n[osmium]\n{siglevel}\nServer = {server}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_missing_is_ok() {
        assert!(remove_file("/definitely/not/here").is_ok());
        assert!(remove_dir("/definitely/not/here").is_ok());
    }

    #[test]
    fn repo_section_has_server() {
        let s = osmium_repo_section("https://x", false);
        assert!(s.contains("[osmium]") && s.contains("Server = https://x") && s.contains("TrustAll"));
        let s = osmium_repo_section("https://x", true);
        assert!(s.contains("SigLevel = Required DatabaseRequired") && !s.contains("TrustAll"));
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_str("a\"b\\c\n\t\u{1}ş"), "\"a\\\"b\\\\c\\n\\t\\u0001ş\"");
        assert_eq!(json_str("\u{1}\u{1f}"), r#""\u0001\u001f""#);
    }

    #[test]
    fn json_objects_skip_nested_values() {
        let text = r#"[{"id": 0, "name": "eDP-1", "description": "BOE \"NE160\" \u00e7", "scale": 1.60,
            "activeWorkspace": { "id": 1, "name": "1" }, "reserved": [0, 40], "focused": true},
            {"name": "HDMI-A-1", "x": -1280}]"#;
        let o = json_objects(text);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0]["name"].as_str(), o[0]["description"].as_str()), ("eDP-1", "BOE \"NE160\" ç"));
        assert_eq!((o[0]["scale"].as_str(), o[0]["focused"].as_str(), o[1]["x"].as_str()), ("1.60", "true", "-1280"));
        assert!(!o[0].contains_key("activeWorkspace") && !o[0].contains_key("reserved"));
        assert!(json_objects("").is_empty() && json_objects("]]}{[[\"a\"").is_empty());
    }
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_interrupt(_: i32) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

/// From now on Ctrl-C (SIGINT), SIGTERM and SIGHUP only set a flag (see `interrupted`), so the
/// tool can clean up. A handler, unlike `SIG_IGN`, is reset by exec: children still die on Ctrl-C.
pub fn catch_interrupts() {
    // Linux numbers: SIGHUP, SIGINT, SIGTERM.
    for sig in [1, 2, 15] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { signal(sig, on_interrupt) };
    }
}

/// True once a signal caught by `catch_interrupts` arrived.
pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}
