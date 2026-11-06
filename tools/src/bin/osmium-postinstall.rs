//! Finishes an OsmiumOS install. Calamares runs it inside the target chroot
//! (shellprocess@postinstall), after users are created and before initcpio.

use osmium_tools::{KEYRING, REPO_URL, Result, append, osmium_repo_section, remove_dir, remove_file, run, succeeds};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Live-ISO files copied over from the squashfs that the installed system must not keep.
const LIVE_FILES: &[&str] = &[
    "/etc/sysusers.d/osmium.conf",
    "/etc/sudoers.d/osmium",
    "/etc/tmpfiles.d/osmium-live.conf",
    "/etc/motd",
    "/etc/mkinitcpio.conf.d/archiso.conf",
    "/etc/ssh/sshd_config.d/10-archiso.conf",
    "/etc/systemd/journald.conf.d/volatile-storage.conf",
    "/etc/systemd/logind.conf.d/do-not-suspend.conf",
    "/etc/systemd/resolved.conf.d/archiso.conf",
    "/etc/systemd/system-generators/systemd-gpt-auto-generator",
    "/etc/systemd/network/20-ethernet.network",
    "/etc/systemd/network/20-wlan.network",
    "/etc/systemd/network/20-wwan.network",
    "/etc/systemd/system/choose-mirror.service",
    "/etc/systemd/system/livecd-alsa-unmuter.service",
    "/etc/systemd/system/livecd-talk.service",
    "/etc/systemd/system/pacman-init.service",
    "/etc/systemd/system/etc-pacman.d-gnupg.mount",
    "/usr/local/bin/Installation_guide",
    "/usr/local/bin/choose-mirror",
    "/usr/local/bin/livecd-sound",
    "/usr/share/applications/osmium-install.desktop",
    "/etc/xdg/autostart/osmium-install.desktop",
    "/etc/sddm.conf.d/autologin.conf",
    "/etc/systemd/system/display-manager.service",
    "/root/.automated_script.sh",
    "/root/.zlogin",
];

const LIVE_DIRS: &[&str] = &[
    "/etc/systemd/system/getty@tty1.service.d",
    "/etc/systemd/system/gdm.service.d",
    "/etc/systemd/system/cloud-init.target.wants",
    "/etc/systemd/system/systemd-networkd-wait-online.service.d",
    "/usr/local/share/livecd-sound",
    "/home/osmium",
];

fn main() {
    if let Err(e) = postinstall() {
        eprintln!("osmium-postinstall: {e}");
        std::process::exit(1);
    }
}

fn postinstall() -> Result {
    for f in LIVE_FILES {
        remove_file(f)?;
    }
    for d in LIVE_DIRS {
        remove_dir(d)?;
    }
    // Units removed above leave dangling enable symlinks.
    remove_dangling_links(Path::new("/etc/systemd/system"))?;

    // The live root has an empty password: lock it, admin goes through sudo (wheel).
    run(Command::new("passwd").args(["-l", "root"]))?;

    // The live keyring lived on tmpfs; the installed system needs its own.
    run(Command::new("pacman-key").arg("--init"))?;
    run(Command::new("pacman-key").args(["--populate", "archlinux"]))?;
    let signed = Path::new(KEYRING).is_file();
    if signed {
        run(Command::new("pacman-key").args(["--populate", "osmium"]))?;
    }

    let conf = fs::read_to_string("/etc/pacman.conf").map_err(|e| format!("/etc/pacman.conf: {e}"))?;
    if !has_osmium_repo(&conf) {
        append("/etc/pacman.conf", &osmium_repo_section(REPO_URL, signed))?;
    }

    // Kernel image for initcpio and the bootloader (the live ISO keeps it outside the squashfs).
    let vmlinuz = fs::read_dir("/usr/lib/modules")
        .map_err(|e| format!("/usr/lib/modules: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path().join("vmlinuz")))
        .find(|p| p.is_file())
        .ok_or("no kernel found in /usr/lib/modules")?;
    copy(&vmlinuz, "/boot/vmlinuz-linux")?;

    let present: Vec<&str> = DMS
        .iter()
        .copied()
        .filter(|dm| Path::new(&format!("/usr/lib/systemd/system/{dm}.service")).exists())
        .collect();
    let dm = pick_dm(&present).ok_or("no display manager (sddm, gdm, greetd) installed")?;
    run(Command::new("systemctl").arg("enable").arg(format!("{dm}.service")))?;
    if dm == "greetd" {
        fs::create_dir_all("/etc/greetd").map_err(|e| format!("/etc/greetd: {e}"))?;
        copy("/usr/share/osmium/greetd.toml", "/etc/greetd/config.toml")?;
    }

    // Snapshots only make sense on a btrfs root.
    if succeeds(Command::new("btrfs").args(["subvolume", "show", "/"])) {
        run(Command::new("snapper").args(["--no-dbus", "-c", "root", "create-config", "/"]))?;
    }
    Ok(())
}

/// Display managers in priority order.
const DMS: &[&str] = &["sddm", "gdm", "greetd"];

/// The display manager to enable: the highest-priority one among `present`.
fn pick_dm<'a>(present: &[&str]) -> Option<&'a str> {
    DMS.iter().copied().find(|d| present.contains(d))
}

fn has_osmium_repo(pacman_conf: &str) -> bool {
    pacman_conf.lines().any(|l| l.trim() == "[osmium]")
}

fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result {
    let (from, to) = (from.as_ref(), to.as_ref());
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))
}

fn remove_dangling_links(dir: &Path) -> Result {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            if fs::metadata(&path).is_err() {
                remove_file(&path)?;
            }
        } else if meta.is_dir() {
            remove_dangling_links(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_priority() {
        assert_eq!(pick_dm(&["greetd"]), Some("greetd"));
        assert_eq!(pick_dm(&["greetd", "gdm"]), Some("gdm"));
        assert_eq!(pick_dm(&["gdm", "sddm", "greetd"]), Some("sddm"));
        assert_eq!(pick_dm(&[]), None);
    }

    #[test]
    fn detects_repo_section() {
        assert!(has_osmium_repo("[core]\n\n[osmium]\nServer = x\n"));
        assert!(!has_osmium_repo("[core]\n#[osmium]\n"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_links_removed_valid_kept() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("osmium-test-{}", std::process::id()));
        let sub = dir.join("a.wants");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.join("real.service"), "").unwrap();
        symlink(dir.join("real.service"), sub.join("ok.service")).unwrap();
        symlink(dir.join("gone.service"), sub.join("bad.service")).unwrap();
        remove_dangling_links(&dir).unwrap();
        assert!(fs::symlink_metadata(sub.join("ok.service")).is_ok());
        assert!(fs::symlink_metadata(sub.join("bad.service")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
