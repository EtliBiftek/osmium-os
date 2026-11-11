//! Throwaway guest session.
//!
//!   osmium-guest start          create the guest and open its desktop on a free VT
//!   osmium-guest cleanup [<vt>] end the session, delete the guest, switch to <vt>
//!   osmium-guest status
//!
//! The guest account `misafir` has a locked password and a tmpfs home, so
//! nothing it does survives. Its desktop runs as the transient system unit
//! osmium-guest.service on its own VT; the current user stays logged in.
//! The unit's ExecStopPost and osmium-guest-cleanup.service (at boot, for
//! crashes) both run `cleanup`, which is idempotent.

use osmium_tools::{Result, exit_on_err, output, run, succeeds};
use std::fs;
use std::process::Command;

const USAGE: &str = "kullanım: osmium-guest start | cleanup [<vt>] | status";
const GUEST: &str = "misafir";
const HOME: &str = "/home/misafir";
/// Marks the account as ours; `cleanup` never deletes an account without it.
const GECOS: &str = "OsmiumOS misafir oturumu";
const UNIT: &str = "osmium-guest.service";
const SELF: &str = "/usr/bin/osmium-guest";

#[derive(Debug, PartialEq)]
enum Account {
    Absent,
    Guest { uid: u32, gid: u32 },
    /// A `misafir` we did not create, or someone else living in /home/misafir.
    Foreign,
}

#[derive(Debug, PartialEq)]
enum HomeState {
    Absent,
    GuestTmpfs,
    EmptyDir,
    /// Holds data on a real filesystem: never touched.
    Real,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match args[..] {
        ["start"] => as_root(&args).and_then(|()| start()),
        ["cleanup"] => as_root(&args).and_then(|()| cleanup(None)),
        ["cleanup", vt] => match parse_vt(vt) {
            Some(vt) => as_root(&args).and_then(|()| cleanup(Some(vt))),
            None => Err(USAGE.into()),
        },
        ["status"] => status(),
        _ => Err(USAGE.into()),
    };
    exit_on_err("osmium-guest", res);
}

/// Classifies the guest account from the contents of /etc/passwd.
fn account(passwd: &str) -> Account {
    let mut found = Account::Absent;
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, gecos, home, _] = f[..] else { continue };
        if name != GUEST && home != HOME {
            continue;
        }
        match (uid.parse::<u32>(), gid.parse::<u32>()) {
            (Ok(uid), Ok(gid)) if name == GUEST && home == HOME && gecos == GECOS && uid >= 1000 => {
                found = Account::Guest { uid, gid }
            }
            _ => return Account::Foreign,
        }
    }
    found
}

/// `fstype` is what is mounted exactly on /home/misafir, if anything.
fn home_state(fstype: Option<&str>, empty: bool) -> HomeState {
    match fstype {
        Some("tmpfs") => HomeState::GuestTmpfs,
        None if empty => HomeState::EmptyDir,
        _ => HomeState::Real,
    }
}

/// `"tty2"` or `"2"` into 2.
fn parse_vt(s: &str) -> Option<u32> {
    let s = s.trim();
    s.strip_prefix("tty").unwrap_or(s).parse().ok().filter(|n| (1..=63).contains(n))
}

/// VTs that have a login session, from `loginctl list-sessions --no-legend`.
fn session_vts(list: &str) -> Vec<u32> {
    list.split_whitespace().filter(|t| t.starts_with("tty")).filter_map(parse_vt).collect()
}

/// Lowest free VT above logind's NAutoVTs (6): switching to a lower one would
/// make logind start a getty on it, racing with the guest session.
fn pick_vt(active: u32, used: &[u32]) -> Option<u32> {
    (7..=12).find(|n| *n != active && !used.contains(n))
}

/// Home directories others can enter or read, from `find -printf "%m %f\n"`.
fn exposed_homes(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(mode, name)| *name != GUEST && u32::from_str_radix(mode, 8).is_ok_and(|m| m & 0o007 != 0))
        .map(|(mode, name)| format!("/home/{name} ({mode})"))
        .collect()
}

/// Deletes what the guest left in the world-writable dirs outside its home.
/// find neither follows symlinks nor leaves the filesystem, and protected_hardlinks
/// keeps the guest from owning a link to someone else's file.
fn shared_files_argv(uid: u32) -> Vec<String> {
    let mut a: Vec<String> = ["find", "/tmp", "/var/tmp", "/dev/shm", "-xdev", "-uid"].map(String::from).to_vec();
    a.push(uid.to_string());
    a.push("-delete".into());
    a
}

/// The guest desktop as a transient system unit. PAMName=login plus a VT as
/// TTYPath gives a logind session on seat0, so the compositor may take the
/// DRM device once that VT is in the foreground.
fn session_argv(vt: u32, prev_vt: u32) -> Vec<String> {
    let mut a: Vec<String> = ["systemd-run", "--quiet", "--collect"].map(String::from).to_vec();
    a.push(format!("--unit={UNIT}"));
    a.push(format!("--description={GECOS}"));
    a.push("--setenv=XDG_SESSION_TYPE=wayland".into());
    for p in [
        format!("User={GUEST}"),
        "PAMName=login".into(),
        "WorkingDirectory=~".into(),
        format!("TTYPath=/dev/tty{vt}"),
        "TTYReset=yes".into(),
        "TTYVHangup=yes".into(),
        "TTYVTDisallocate=yes".into(),
        "StandardInput=tty-fail".into(),
        "StandardOutput=journal".into(),
        "StandardError=journal".into(),
        format!("UtmpIdentifier=tty{vt}"),
        "UtmpMode=user".into(),
        format!("ExecStartPre=+/usr/bin/chvt {vt}"),
        format!("ExecStopPost=+{SELF} cleanup {prev_vt}"),
    ] {
        a.push(format!("--property={p}"));
    }
    a.extend(["--", "/usr/bin/osmium-dots", "session"].map(String::from));
    a
}

/// Re-runs the tool through sudo when not root; returns only as root.
fn as_root(args: &[&str]) -> Result {
    if output(Command::new("id").arg("-u"))?.trim() == "0" {
        return Ok(());
    }
    let status = Command::new("sudo").arg(SELF).args(args).status().map_err(|e| format!("sudo: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn unit_active() -> bool {
    succeeds(Command::new("systemctl").args(["is-active", "--quiet", UNIT]))
}

fn read_account() -> Result<Account> {
    fs::read_to_string("/etc/passwd").map(|p| account(&p)).map_err(|e| format!("/etc/passwd: {e}"))
}

fn probe_home() -> HomeState {
    let Ok(meta) = fs::symlink_metadata(HOME) else { return HomeState::Absent };
    if !meta.is_dir() {
        return HomeState::Real;
    }
    let fstype = output(Command::new("findmnt").args(["-n", "-o", "FSTYPE", "--mountpoint", HOME])).ok();
    let empty = fs::read_dir(HOME).is_ok_and(|mut d| d.next().is_none());
    home_state(fstype.as_deref().map(str::trim).filter(|s| !s.is_empty()), empty)
}

/// Deletes the guest's processes, home and account. Safe to repeat.
fn remove_leftovers() -> Result {
    let acct = read_account()?;
    if acct == Account::Foreign {
        return Err(format!("`{GUEST}` hesabı bu araca ait değil; hiçbir şeye dokunulmadı"));
    }
    let home = probe_home();
    if home == HomeState::Real {
        return Err(format!("{HOME} gerçek bir dizin (tmpfs değil); hiçbir şeye dokunulmadı"));
    }
    if let Account::Guest { uid, .. } = acct {
        succeeds(Command::new("loginctl").args(["terminate-user", GUEST]));
        succeeds(Command::new("pkill").args(["-KILL", "-U", &uid.to_string()]));
        // The next guest usually gets the same uid and would inherit these files.
        // Best effort: a guest dir holding another user's file must not block the cleanup.
        let argv = shared_files_argv(uid);
        succeeds(Command::new(&argv[0]).args(&argv[1..]));
    }
    if home == HomeState::GuestTmpfs {
        // Lazy: the processes killed above may still hold files for a moment.
        run(Command::new("umount").args(["-l", HOME]))?;
    }
    if acct != Account::Absent {
        run(Command::new("userdel").args(["--force", GUEST]))?;
    }
    if home != HomeState::Absent {
        // Not recursive on purpose: anything left under the mount is not ours.
        fs::remove_dir(HOME).map_err(|e| format!("{HOME}: {e}"))?;
    }
    Ok(())
}

fn start() -> Result {
    if unit_active() {
        return Err("misafir oturumu zaten açık (bkz: osmium-guest status)".into());
    }
    remove_leftovers()?;

    let active = fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()
        .and_then(|s| parse_vt(&s))
        .ok_or("etkin sanal konsol okunamadı")?;
    let sessions = output(Command::new("loginctl").args(["list-sessions", "--no-legend"]))?;
    let vt = pick_vt(active, &session_vts(&sessions)).ok_or("boş sanal konsol yok (tty7-tty12 dolu)")?;

    let homes = output(Command::new("find").args(["/home", "-mindepth", "1", "-maxdepth", "1", "-type", "d", "-printf", "%m %f\\n"]))?;
    let exposed = exposed_homes(&homes);
    if !exposed.is_empty() {
        eprintln!(
            "UYARI: misafir şu ev dizinlerini okuyabilir: {}\n       Kapatmak için: chmod 700 <dizin>",
            exposed.join(", ")
        );
    }

    run(Command::new("useradd").args([
        "--no-create-home",
        "--home-dir",
        HOME,
        "--comment",
        GECOS,
        "--shell",
        "/bin/bash",
        "--user-group",
        "--groups",
        "video,audio,input",
        GUEST,
    ]))?;
    let res = launch(vt, active);
    if res.is_err() {
        // Best effort: the launch error is the one worth reporting.
        let _ = remove_leftovers();
    }
    res
}

fn launch(vt: u32, active: u32) -> Result {
    let Account::Guest { uid, gid } = read_account()? else {
        return Err(format!("`{GUEST}` hesabı oluşturulamadı"));
    };
    fs::create_dir(HOME).map_err(|e| format!("{HOME}: {e}"))?;
    let opts = format!("size=50%,mode=0700,uid={uid},gid={gid},nosuid,nodev");
    run(Command::new("mount").args(["-t", "tmpfs", "-o", &opts, "tmpfs", HOME]))?;
    // Copied by the guest itself, so the files are its own without a chown pass.
    run(Command::new("runuser").args(["-u", GUEST, "--", "cp", "-rT", "/etc/skel", HOME]))?;

    // The guest could otherwise switch back to an unlocked desktop.
    lock_desktops(uid)?;
    println!(
        "Misafir oturumu tty{vt} üzerinde açılıyor. Oturum kapanınca misafirin tüm verisi silinir \
         ve tty{active} konsoluna dönülür."
    );
    let argv = session_argv(vt, active);
    run(Command::new(&argv[0]).args(&argv[1..]))
}

/// `hyprctl locked` reply: None when no compositor answered.
fn parse_locked(reply: &str) -> Option<bool> {
    match reply.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Compositor instances of other users: (runtime dir, instance signature).
fn desktops(guest_uid: u32) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for user in fs::read_dir("/run/user").into_iter().flatten().flatten() {
        let name = user.file_name().to_string_lossy().into_owned();
        if name.parse::<u32>().map_or(true, |uid| uid == guest_uid) {
            continue;
        }
        let runtime = format!("/run/user/{name}");
        for inst in fs::read_dir(format!("{runtime}/hypr")).into_iter().flatten().flatten() {
            if inst.path().join(".socket.sock").exists() {
                out.push((runtime.clone(), inst.file_name().to_string_lossy().into_owned()));
            }
        }
    }
    out
}

/// Root reaches every user's socket; the dispatched locker runs as that user, in that session.
fn hyprctl_on(runtime: &str, sig: &str, args: &[&str]) -> String {
    Command::new("hyprctl")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("HYPRLAND_INSTANCE_SIGNATURE", sig)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Desktops that still answer `locked = false`.
fn unlocked(desktops: &[(String, String)]) -> Vec<(String, String)> {
    desktops.iter().filter(|(r, s)| parse_locked(&hyprctl_on(r, s, &["locked"])) == Some(false)).cloned().collect()
}

/// Locks every other desktop, and checks it through the compositor: `loginctl lock-sessions`
/// only asks, and nothing locks when the session has no lock handler (hypridle).
/// Fails when a desktop stays unlocked, so the guest never starts next to one.
fn lock_desktops(guest_uid: u32) -> Result {
    succeeds(Command::new("loginctl").arg("lock-sessions"));
    let all = desktops(guest_uid);
    let wait = |rounds: u32| {
        for _ in 0..rounds {
            if unlocked(&all).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        unlocked(&all)
    };
    let left = wait(4);
    for (r, s) in &left {
        // hypridle's own lock command uses the same guard against a second locker.
        hyprctl_on(r, s, &["dispatch", "exec", "pidof hyprlock || hyprlock"]);
    }
    match wait(if left.is_empty() { 0 } else { 20 }).len() {
        0 => Ok(()),
        n => Err(format!(
            "{n} masaüstü kilitlenemedi; misafir oturumu açılmadı (misafir Ctrl+Alt+F ile o masaüstüne geçebilirdi). \
             Ekranı kilitleyip yeniden deneyin."
        )),
    }
}

fn cleanup(vt: Option<u32>) -> Result {
    if unit_active() {
        println!("Misafir oturumu açık: kapatılacak ve misafirin tüm verisi silinecek.");
        print!("Onaylamak için 'evet' yazın: ");
        let mut answer = String::new();
        {
            use std::io::Write;
            std::io::stdout().flush().map_err(|e| e.to_string())?;
        }
        std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
        if answer.trim() != "evet" {
            return Err("iptal edildi".into());
        }
        // The unit's own ExecStopPost cleans up and switches the VT back.
        run(Command::new("systemctl").args(["stop", UNIT]))?;
    }
    let res = remove_leftovers();
    // Switch back even when cleanup failed, but report the cleanup error first.
    let switched = vt.map_or(Ok(()), |vt| run(Command::new("chvt").arg(vt.to_string())));
    res.and(switched)
}

fn status() -> Result {
    let tty = output(Command::new("systemctl").args(["show", "--property=TTYPath", "--value", UNIT])).unwrap_or_default();
    match (unit_active(), tty.trim()) {
        (true, "") => println!("Oturum: açık"),
        (true, tty) => println!("Oturum: açık ({tty})"),
        (false, _) => println!("Oturum: kapalı"),
    }
    println!(
        "Hesap:  {}",
        match read_account()? {
            Account::Absent => "yok".to_string(),
            Account::Guest { uid, .. } => format!("{GUEST} (uid {uid})"),
            Account::Foreign => format!("`{GUEST}` var ama bu araca ait değil"),
        }
    );
    println!(
        "Ev:     {}",
        match probe_home() {
            HomeState::Absent => "yok",
            HomeState::GuestTmpfs => "tmpfs (geçici)",
            HomeState::EmptyDir => "boş dizin (artık; cleanup siler)",
            HomeState::Real => "gerçek dizin: misafir oturumu açılamaz",
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_lock_state() {
        assert_eq!(parse_locked("true\n"), Some(true));
        assert_eq!(parse_locked("false"), Some(false));
        assert_eq!(parse_locked(""), None);
        assert_eq!(parse_locked("HYPRLAND_INSTANCE_SIGNATURE was not set"), None);
    }

    const ROOT: &str = "root:x:0:0::/root:/usr/bin/bash\nali:x:1000:1000:Ali:/home/ali:/bin/bash\n";

    fn with(line: &str) -> String {
        format!("{ROOT}{line}\n")
    }

    #[test]
    fn account_absent_or_ours() {
        assert_eq!(account(ROOT), Account::Absent);
        assert_eq!(account(""), Account::Absent);
        let ours = with(&format!("misafir:x:1001:1002:{GECOS}:/home/misafir:/bin/bash"));
        assert_eq!(account(&ours), Account::Guest { uid: 1001, gid: 1002 });
    }

    #[test]
    fn account_foreign_is_never_ours() {
        for line in [
            "misafir:x:1001:1001:Misafir Bey:/home/misafir:/bin/bash".to_string(),
            "misafir:x:1001:1001::/home/misafir:/bin/bash".to_string(),
            format!("misafir:x:1001:1001:{GECOS}:/home/baska:/bin/bash"),
            format!("misafir:x:0:0:{GECOS}:/home/misafir:/bin/bash"),
            format!("misafir:x:999:999:{GECOS}:/home/misafir:/bin/bash"),
            format!("misafir:x:abc:1001:{GECOS}:/home/misafir:/bin/bash"),
            format!("veli:x:1001:1001:{GECOS}:/home/misafir:/bin/bash"),
        ] {
            assert_eq!(account(&with(&line)), Account::Foreign, "{line}");
        }
        // A foreign line wins even when our own entry is also present.
        let both = format!("misafir:x:1001:1001:{GECOS}:/home/misafir:/bin/bash\nveli:x:1002:1002::/home/misafir:/bin/sh\n");
        assert_eq!(account(&both), Account::Foreign);
    }

    #[test]
    fn similar_names_are_ignored() {
        assert_eq!(account(&with("misafir2:x:1001:1001::/home/misafir2:/bin/bash")), Account::Absent);
    }

    #[test]
    fn home_states() {
        assert_eq!(home_state(Some("tmpfs"), false), HomeState::GuestTmpfs);
        assert_eq!(home_state(Some("tmpfs"), true), HomeState::GuestTmpfs);
        assert_eq!(home_state(None, true), HomeState::EmptyDir);
        assert_eq!(home_state(None, false), HomeState::Real);
        assert_eq!(home_state(Some("btrfs"), true), HomeState::Real);
    }

    #[test]
    fn vt_parsing() {
        assert_eq!(parse_vt("tty2\n"), Some(2));
        assert_eq!(parse_vt("7"), Some(7));
        for bad in ["", "0", "64", "tty", "pts/0", "-1", "2; reboot", "ttyS0"] {
            assert_eq!(parse_vt(bad), None, "{bad}");
        }
    }

    #[test]
    fn vts_with_sessions() {
        let list = "      1 1000 ali     seat0 tty2  active no  -\n     c1  965 greeter seat0 tty1  closing no -\n      4 1000 ali     -     pts/0 active no -\n";
        assert_eq!(session_vts(list), [2, 1]);
        assert!(session_vts("").is_empty());
    }

    #[test]
    fn vt_choice() {
        assert_eq!(pick_vt(2, &[1, 2]), Some(7));
        assert_eq!(pick_vt(7, &[1]), Some(8));
        assert_eq!(pick_vt(1, &[7, 8]), Some(9));
        assert_eq!(pick_vt(1, &[7, 8, 9, 10, 11, 12]), None);
    }

    #[test]
    fn exposed_home_listing() {
        let listing = "700 ali\n755 veli\n750 ayse\n711 can\n755 misafir\n1777 ortak alan\nbozuk\n";
        assert_eq!(exposed_homes(listing), ["/home/veli (755)", "/home/can (711)", "/home/ortak alan (1777)"]);
        assert!(exposed_homes("700 ali\n").is_empty());
    }

    #[test]
    fn session_unit_argv() {
        assert_eq!(
            session_argv(7, 2),
            [
                "systemd-run",
                "--quiet",
                "--collect",
                "--unit=osmium-guest.service",
                "--description=OsmiumOS misafir oturumu",
                "--setenv=XDG_SESSION_TYPE=wayland",
                "--property=User=misafir",
                "--property=PAMName=login",
                "--property=WorkingDirectory=~",
                "--property=TTYPath=/dev/tty7",
                "--property=TTYReset=yes",
                "--property=TTYVHangup=yes",
                "--property=TTYVTDisallocate=yes",
                "--property=StandardInput=tty-fail",
                "--property=StandardOutput=journal",
                "--property=StandardError=journal",
                "--property=UtmpIdentifier=tty7",
                "--property=UtmpMode=user",
                "--property=ExecStartPre=+/usr/bin/chvt 7",
                "--property=ExecStopPost=+/usr/bin/osmium-guest cleanup 2",
                "--",
                "/usr/bin/osmium-dots",
                "session",
            ]
        );
    }

    #[test]
    fn shared_dirs_cleanup_targets_only_the_guest_uid() {
        assert_eq!(
            shared_files_argv(1001),
            ["find", "/tmp", "/var/tmp", "/dev/shm", "-xdev", "-uid", "1001", "-delete"]
        );
    }

    #[test]
    fn boot_cleanup_unit_calls_cleanup() {
        let unit = include_str!("../../data/systemd-system/osmium-guest-cleanup.service");
        assert!(unit.contains("ExecStart=/usr/bin/osmium-guest cleanup\n"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }
}
