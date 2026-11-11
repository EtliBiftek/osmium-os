//! Tries packages without leaving a trace.
//!
//!   osmium-try <package>... [-- <command> [args]...]
//!
//! `systemd-nspawn --ephemeral -D /` runs on a btrfs snapshot of the running
//! root that is deleted on exit. The packages are installed inside it and the
//! command (default: the first package's name) runs as the invoking user with
//! the Wayland/PipeWire/X11 sockets bound in, so GUI apps show up on the real
//! desktop. /home is a separate subvolume (@home) and therefore empty in the
//! snapshot; the user gets a tmpfs home instead. Only the pacman package cache
//! is shared read-write, so repeated tries do not download again.

use osmium_tools::{Result, exit_on_err, output, succeeds};
use std::path::Path;
use std::process::Command;

const USAGE: &str = "kullanım: osmium-try <paket>... [-- <komut> [argüman]...]";
const PKG_CACHE: &str = "/var/cache/pacman/pkg";
const X11_DIR: &str = "/tmp/.X11-unix";
const DRI_DIR: &str = "/dev/dri";
const PASS_ENV: [&str; 4] = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DISPLAY", "LANG"];

/// Runs as root inside the container: `$1` package count, `$2` user, then the
/// packages, then the command. Everything arrives as positional parameters,
/// so no user text is ever parsed by the shell.
const INNER: &str = r#"n=$1 user=$2; shift 2; pacman -S --noconfirm --needed -- "${@:1:n}" || exit; shift "$n"; exec runuser -u "$user" -- "$@""#;

#[derive(Debug, PartialEq)]
struct Invoker {
    user: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// What exists on the host and gets passed into the container.
#[derive(Debug, Default)]
struct Host {
    /// `$XDG_RUNTIME_DIR`, when usable.
    runtime_dir: Option<String>,
    /// Socket paths relative to `runtime_dir` that exist.
    sockets: Vec<String>,
    x11: bool,
    dri: bool,
    /// NVIDIA proprietary device nodes (`/dev/nvidia*`, `/dev/nvidia-caps/*`).
    nvidia: Vec<String>,
    env: Vec<(String, String)>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    exit_on_err("osmium-try", try_packages(&args));
}

/// pacman package names: alphanumerics and `@ . _ + -`, not starting with `-` or `.`.
fn valid_package(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c))
}

/// Splits the command line into (packages, command).
fn parse_args(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let (packages, command) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], Some(&args[i + 1..])),
        None => (args, None),
    };
    if packages.is_empty() || command.is_some_and(|c| c.is_empty()) {
        return Err(USAGE.into());
    }
    if let Some(bad) = packages.iter().find(|p| !valid_package(p)) {
        return Err(format!("geçersiz paket adı: `{bad}`\n{USAGE}"));
    }
    let command = command.map_or_else(|| vec![packages[0].clone()], <[String]>::to_vec);
    Ok((packages.to_vec(), command))
}

/// nspawn splits `--bind`/`--tmpfs` values on `:` and unescapes `\`.
fn mountable(path: &str) -> bool {
    path.starts_with('/') && !path.contains([':', '\\'])
}

fn valid_user(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c))
}

/// One `getent passwd` line into the invoking user.
fn parse_passwd_line(line: &str) -> Option<Invoker> {
    let f: Vec<&str> = line.trim().split(':').collect();
    let [user, _, uid, gid, _, home, _] = f[..] else { return None };
    let who = Invoker { user: user.into(), uid: uid.parse().ok()?, gid: gid.parse().ok()?, home: home.into() };
    (valid_user(&who.user) && mountable(&who.home)).then_some(who)
}

/// Sockets under `$XDG_RUNTIME_DIR` worth binding, if they exist.
fn candidate_sockets(wayland_display: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    // WAYLAND_DISPLAY may also be an absolute path; only the plain-name form is under the runtime dir.
    if let Some(w) = wayland_display.filter(|w| !w.is_empty() && !w.contains(['/', ':', '\\'])) {
        v.push(w.to_string());
    }
    v.extend(["pipewire-0".to_string(), "pulse/native".to_string()]);
    v
}

fn nspawn_argv(packages: &[String], command: &[String], who: &Invoker, host: &Host) -> Vec<String> {
    let owned = format!("mode=0700,uid={},gid={}", who.uid, who.gid);
    let mut a: Vec<String> = [
        "systemd-nspawn",
        "--quiet",
        "--ephemeral",
        "--directory=/",
        // No --machine: nspawn then names an ephemeral container <hostname>-<random>,
        // so two tries can run at once (a fixed name collides on the scope unit).
        "--register=no",
        // bash must not be PID 1: it would ignore SIGTERM/SIGINT and not reap.
        "--as-pid2",
    ]
    .map(String::from)
    .to_vec();
    a.push(format!("--tmpfs={}:{owned}", who.home));
    // The default cwd is /root, which the user cannot write to or even enter.
    a.push(format!("--chdir={}", who.home));
    a.push(format!("--bind={PKG_CACHE}"));
    if let Some(rt) = &host.runtime_dir {
        // /run is fresh in the container; apps need a runtime dir they own.
        a.push(format!("--tmpfs={rt}:{owned}"));
        a.extend(host.sockets.iter().map(|s| format!("--bind={rt}/{s}")));
    }
    if host.x11 {
        a.push(format!("--bind-ro={X11_DIR}"));
    }
    if host.dri {
        a.push(format!("--bind={DRI_DIR}"));
        a.push("--property=DeviceAllow=char-drm rw".into());
    }
    // NVIDIA's proprietary driver needs its own nodes next to the DRM ones, or GL
    // falls back to software rendering.
    for node in host.nvidia.iter().filter(|n| mountable(n)) {
        a.push(format!("--bind={node}"));
        a.push(format!("--property=DeviceAllow={node} rw"));
    }
    a.extend(host.env.iter().map(|(k, v)| format!("--setenv={k}={v}")));
    a.extend(["--", "/usr/bin/bash", "-c", INNER, "osmium-try"].map(String::from));
    a.push(packages.len().to_string());
    a.push(who.user.clone());
    a.extend_from_slice(packages);
    a.extend_from_slice(command);
    a
}

fn probe_host() -> Host {
    let env: Vec<(String, String)> =
        PASS_ENV.iter().filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?))).collect();
    let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let runtime_dir = get("XDG_RUNTIME_DIR").filter(|d| mountable(d) && Path::new(d).is_dir()).map(String::from);
    let sockets = match &runtime_dir {
        Some(rt) => candidate_sockets(get("WAYLAND_DISPLAY"))
            .into_iter()
            .filter(|s| Path::new(rt).join(s).exists())
            .collect(),
        None => Vec::new(),
    };
    let nvidia = nvidia_nodes(&list_dir("/dev"), &list_dir("/dev/nvidia-caps"));
    Host { runtime_dir, sockets, x11: Path::new(X11_DIR).is_dir(), dri: Path::new(DRI_DIR).is_dir(), nvidia, env }
}

fn list_dir(dir: &str) -> Vec<String> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries.map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

/// `/dev/nvidia0`, `nvidiactl`, `nvidia-modeset`, `nvidia-uvm(-tools)` and the
/// capability nodes, from the names in /dev and /dev/nvidia-caps. Sorted.
fn nvidia_nodes(dev: &[String], caps: &[String]) -> Vec<String> {
    let mut v: Vec<String> = dev
        .iter()
        .filter(|n| n.starts_with("nvidia") && *n != "nvidia-caps")
        .map(|n| format!("/dev/{n}"))
        .chain(caps.iter().filter(|n| n.starts_with("nvidia-cap")).map(|n| format!("/dev/nvidia-caps/{n}")))
        .collect();
    v.sort();
    v
}

fn try_packages(args: &[String]) -> Result {
    let (packages, command) = parse_args(args)?;

    let uid = output(Command::new("id").arg("-u"))?;
    let uid = uid.trim();
    if uid == "0" {
        return Err("root olarak değil, normal kullanıcı olarak çalıştırın (sudo kendisi çağrılır)".into());
    }
    let who = parse_passwd_line(&output(Command::new("getent").args(["passwd", uid]))?)
        .ok_or("kullanıcı bilgisi okunamadı (getent passwd)")?;

    if !succeeds(Command::new("systemd-nspawn").arg("--version")) {
        return Err("systemd-nspawn bulunamadı (systemd paketiyle gelir)".into());
    }
    // On other filesystems nspawn would silently copy the whole root instead of snapshotting it.
    let fstype = output(Command::new("findmnt").args(["-n", "-o", "FSTYPE", "/"]))?;
    if fstype.trim() != "btrfs" {
        return Err(format!(
            "kök dosya sistemi btrfs değil ({}): geçici anlık görüntü alınamaz",
            fstype.trim()
        ));
    }

    println!("Geçici ortam hazırlanıyor: {} (sudo parolası istenebilir)", packages.join(" "));
    let argv = nspawn_argv(&packages, &command, &who, &probe_host());
    let status = Command::new("sudo").args(&argv).status().map_err(|e| format!("sudo: {e}"))?;

    println!(
        "Geçici ortam silindi. Atılanlar: kurulan paketler ({}), sistemde yapılan tüm değişiklikler \
         ve geçici ev dizinine yazılan her şey.\nKalan tek şey: {PKG_CACHE} içindeki indirilmiş paket dosyaları.",
        packages.join(", ")
    );
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn who() -> Invoker {
        Invoker { user: "ali".into(), uid: 1000, gid: 1001, home: "/home/ali".into() }
    }

    fn full_host() -> Host {
        Host {
            runtime_dir: Some("/run/user/1000".into()),
            sockets: strings(&["wayland-1", "pipewire-0", "pulse/native"]),
            x11: true,
            dri: true,
            nvidia: Vec::new(),
            env: vec![
                ("WAYLAND_DISPLAY".into(), "wayland-1".into()),
                ("XDG_RUNTIME_DIR".into(), "/run/user/1000".into()),
                ("LANG".into(), "tr_TR.UTF-8".into()),
            ],
        }
    }

    #[test]
    fn finds_nvidia_nodes() {
        let dev = strings(&["nvidia0", "nvidiactl", "nvidia-modeset", "nvidia-uvm", "nvidia-caps", "dri", "null"]);
        assert_eq!(
            nvidia_nodes(&dev, &strings(&["nvidia-cap1", "nvidia-cap2"])),
            [
                "/dev/nvidia-caps/nvidia-cap1",
                "/dev/nvidia-caps/nvidia-cap2",
                "/dev/nvidia-modeset",
                "/dev/nvidia-uvm",
                "/dev/nvidia0",
                "/dev/nvidiactl"
            ]
        );
        assert!(nvidia_nodes(&strings(&["dri", "null"]), &[]).is_empty());
    }

    #[test]
    fn passes_nvidia_nodes() {
        let host = Host { nvidia: strings(&["/dev/nvidiactl", "/dev/nvidia0"]), ..full_host() };
        let a = nspawn_argv(&strings(&["glxgears"]), &strings(&["glxgears"]), &who(), &host);
        for x in ["--bind=/dev/nvidiactl", "--property=DeviceAllow=/dev/nvidiactl rw", "--bind=/dev/nvidia0"] {
            assert!(a.contains(&x.to_string()), "{x}: {a:?}");
        }
    }

    #[test]
    fn package_names() {
        for ok in ["firefox", "gtk4", "libc++", "python-pip", "r8168-dkms", "qt5.15", "foo@bar", "A_b"] {
            assert!(valid_package(ok), "{ok}");
        }
        for bad in ["", "-Syu", "--overwrite=*", ".hidden", "a b", "a;b", "a/b", "$(x)", "a\nb", "paket=1", "çay"] {
            assert!(!valid_package(bad), "{bad}");
        }
    }

    #[test]
    fn args_default_command_is_first_package() {
        let (p, c) = parse_args(&strings(&["gimp", "krita"])).unwrap();
        assert_eq!(p, ["gimp", "krita"]);
        assert_eq!(c, ["gimp"]);
    }

    #[test]
    fn args_explicit_command_keeps_its_own_dashes() {
        let (p, c) = parse_args(&strings(&["imagemagick", "--", "magick", "--version", "--", "x"])).unwrap();
        assert_eq!(p, ["imagemagick"]);
        assert_eq!(c, ["magick", "--version", "--", "x"]);
    }

    #[test]
    fn args_rejected() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&strings(&["--", "sh"])).is_err(), "no package");
        assert!(parse_args(&strings(&["gimp", "--"])).is_err(), "empty command");
        assert!(parse_args(&strings(&["gimp", "--noconfirm"])).unwrap_err().contains("--noconfirm"));
        assert!(parse_args(&strings(&["gimp; rm -rf /"])).is_err());
    }

    #[test]
    fn passwd_line() {
        assert_eq!(parse_passwd_line("ali:x:1000:1001:Ali Veli:/home/ali:/bin/bash\n"), Some(who()));
        assert_eq!(parse_passwd_line("ali:x:1000:1001::/home/a:b:/bin/bash"), None, "colon in home");
        assert_eq!(parse_passwd_line("ali:x:abc:1001::/home/ali:/bin/bash"), None);
        assert_eq!(parse_passwd_line("Ali Veli:x:1000:1001::/home/ali:/bin/bash"), None);
        assert_eq!(parse_passwd_line("ali:x:1000:1001::home/ali:/bin/bash"), None, "relative home");
        assert_eq!(parse_passwd_line(""), None);
    }

    #[test]
    fn socket_candidates() {
        assert_eq!(candidate_sockets(Some("wayland-1")), ["wayland-1", "pipewire-0", "pulse/native"]);
        assert_eq!(candidate_sockets(None), ["pipewire-0", "pulse/native"]);
        assert_eq!(candidate_sockets(Some("/run/user/1000/wayland-1")).len(), 2);
        assert_eq!(candidate_sockets(Some("a:b")).len(), 2);
        assert_eq!(candidate_sockets(Some("")).len(), 2);
    }

    #[test]
    fn argv_full() {
        let a = nspawn_argv(&strings(&["gimp", "krita"]), &strings(&["gimp", "--new", "a b"]), &who(), &full_host());
        assert_eq!(
            a,
            [
                "systemd-nspawn",
                "--quiet",
                "--ephemeral",
                "--directory=/",
                "--register=no",
                "--as-pid2",
                "--tmpfs=/home/ali:mode=0700,uid=1000,gid=1001",
                "--chdir=/home/ali",
                "--bind=/var/cache/pacman/pkg",
                "--tmpfs=/run/user/1000:mode=0700,uid=1000,gid=1001",
                "--bind=/run/user/1000/wayland-1",
                "--bind=/run/user/1000/pipewire-0",
                "--bind=/run/user/1000/pulse/native",
                "--bind-ro=/tmp/.X11-unix",
                "--bind=/dev/dri",
                "--property=DeviceAllow=char-drm rw",
                "--setenv=WAYLAND_DISPLAY=wayland-1",
                "--setenv=XDG_RUNTIME_DIR=/run/user/1000",
                "--setenv=LANG=tr_TR.UTF-8",
                "--",
                "/usr/bin/bash",
                "-c",
                INNER,
                "osmium-try",
                "2",
                "ali",
                "gimp",
                "krita",
                "gimp",
                "--new",
                "a b",
            ]
        );
    }

    #[test]
    fn argv_bare_host() {
        let a = nspawn_argv(&strings(&["htop"]), &strings(&["htop"]), &who(), &Host::default());
        assert!(a.contains(&"--tmpfs=/home/ali:mode=0700,uid=1000,gid=1001".to_string()));
        assert!(a.contains(&"--bind=/var/cache/pacman/pkg".to_string()));
        assert_eq!(a.iter().filter(|x| x.starts_with("--bind")).count(), 1, "{a:?}");
        assert_eq!(a.iter().filter(|x| x.starts_with("--tmpfs")).count(), 1, "{a:?}");
        assert!(!a.iter().any(|x| x.starts_with("--setenv") || x.starts_with("--property")));
        assert_eq!(a[a.len() - 4..], ["1", "ali", "htop", "htop"]);
    }

    #[test]
    fn argv_keeps_network_and_never_binds_real_home() {
        let a = nspawn_argv(&strings(&["gimp"]), &strings(&["gimp"]), &who(), &full_host());
        assert!(!a.iter().any(|x| x.contains("private-network")));
        assert!(!a.iter().any(|x| x.starts_with("--bind") && x.contains("/home")));
    }

    #[test]
    fn argv_allows_concurrent_tries_and_starts_in_the_tmpfs_home() {
        let a = nspawn_argv(&strings(&["htop"]), &strings(&["htop"]), &who(), &Host::default());
        assert!(!a.iter().any(|x| x.starts_with("--machine") || x == "-M"), "{a:?}");
        assert!(a.contains(&"--chdir=/home/ali".to_string()));
    }

    #[test]
    fn hostile_command_stays_one_argument() {
        let cmd = strings(&["sh", "-c", "echo $(id); rm -rf /"]);
        let a = nspawn_argv(&strings(&["dash"]), &cmd, &who(), &Host::default());
        let script = a.iter().position(|x| x == INNER).unwrap();
        assert_eq!(a[script - 1], "-c");
        assert_eq!(a[script + 1..], ["osmium-try", "1", "ali", "dash", "sh", "-c", "echo $(id); rm -rf /"]);
        // The script itself is constant: nothing from the user is spliced into it.
        assert!(!INNER.contains("dash"));
    }

    #[test]
    fn mountable_paths() {
        assert!(mountable("/run/user/1000"));
        assert!(!mountable("run/user"));
        assert!(!mountable("/a:b"));
        assert!(!mountable("/a\\b"));
    }
}
