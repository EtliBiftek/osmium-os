//! Shared helpers for OsmiumOS tools. Std only: no crates to fetch at build time.

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
}
