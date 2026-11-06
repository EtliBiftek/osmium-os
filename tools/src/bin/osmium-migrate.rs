//! Windows migration wizard. Reads a Windows install (mounted read-only) and
//! copies what can be copied; nothing is ever written to or deleted from Windows.
//!
//!   osmium-migrate scan                 find NTFS partitions, mount ro, list users
//!   osmium-migrate files <winuser>      Documents, Pictures, Music, Videos, Desktop, Downloads
//!   osmium-migrate wallpaper <winuser>  copy the current Windows wallpaper
//!   osmium-migrate fonts <winuser>      copy the user's own fonts (not system fonts)
//!   osmium-migrate wifi                 import Wi-Fi names (no passwords) into NetworkManager
//!   osmium-migrate apps [--install]     map installed Windows programs to Linux ones
//!   osmium-migrate all <winuser>        every step above, one confirmation each
//!
//! Every write asks for `evet` after a dry-run summary. Existing files are skipped.

use osmium_tools::{Result, data_home, exit_on_err, output, run, succeeds};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const MOUNT_ROOT: &str = "/run/osmium-migrate";
/// (Windows folder, xdg-user-dir key)
const KNOWN_DIRS: &[(&str, &str)] = &[
    ("Documents", "DOCUMENTS"),
    ("Pictures", "PICTURES"),
    ("Music", "MUSIC"),
    ("Videos", "VIDEOS"),
    ("Desktop", "DESKTOP"),
    ("Downloads", "DOWNLOAD"),
];
const SYSTEM_USERS: &[&str] = &["default", "public", "all users", "default user", "defaultuser0", "wdagutilityaccount"];
/// Windows housekeeping files nobody wants on Linux.
const JUNK_FILES: &[&str] = &["desktop.ini", "thumbs.db"];
/// Program Files / Start Menu entries that are not user programs (lowercase).
const IGNORED_APPS: &[&str] = &[
    "common files", "windows defender", "windows mail", "windows media player", "windows nt",
    "windows photo viewer", "windows sidebar", "windowsapps", "modifiablewindowsapps",
    "internet explorer", "microsoft.net", "reference assemblies", "msbuild", "dotnet",
    "windowspowershell", "windows security", "uninstall information", "accessories",
    "administrative tools", "system tools", "windows powershell", "startup", "maintenance",
    "accessibility", "windows accessories", "windows system", "windows kits", "windows portable devices",
];

// ---------- partitions ----------

#[derive(Debug, PartialEq)]
struct Part {
    name: String,
    fstype: String,
    label: String,
    size: String,
    mount: String,
}

struct Found {
    part: Part,
    mount: PathBuf,
    windows: bool,
    users: Vec<String>,
}

/// lsblk -r escapes spaces and such as \xNN.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `lsblk -rno NAME,FSTYPE,LABEL,SIZE,MOUNTPOINT`; keeps NTFS only.
/// Split on single spaces: an empty label or mountpoint is an empty field.
fn parse_lsblk(text: &str) -> Vec<Part> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            if f.len() < 4 || !matches!(f[1], "ntfs" | "ntfs3") {
                return None;
            }
            Some(Part {
                name: f[0].into(),
                fstype: f[1].into(),
                label: unescape(f[2]),
                size: f[3].into(),
                mount: f.get(4).copied().map(unescape).unwrap_or_default(),
            })
        })
        .collect()
}

fn ensure_mounted(p: &Part) -> Result<PathBuf> {
    if !p.mount.is_empty() {
        return Ok(PathBuf::from(&p.mount));
    }
    let dir = Path::new(MOUNT_ROOT).join(&p.name);
    let dev = format!("/dev/{}", p.name);
    run(Command::new("sudo").arg("mkdir").arg("-p").arg(&dir))?;
    // Always ro: the Windows disk must never be modified.
    let ok = ["ntfs3", "ntfs-3g"]
        .iter()
        .any(|t| succeeds(Command::new("sudo").args(["mount", "-o", "ro", "-t", t]).arg(&dev).arg(&dir)));
    if ok {
        Ok(dir)
    } else {
        Err(format!("{dev} salt okunur bağlanamadı (Windows'ta hızlı başlatma/hazırda bekletme açık olabilir)"))
    }
}

fn is_system_user(name: &str) -> bool {
    SYSTEM_USERS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

fn list_users(users_dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(users_dir) else { return Vec::new() };
    let mut v: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !is_system_user(n))
        .collect();
    v.sort();
    v
}

fn discover() -> Result<Vec<Found>> {
    let text = output(Command::new("lsblk").args(["-rno", "NAME,FSTYPE,LABEL,SIZE,MOUNTPOINT"]))?;
    let mut found = Vec::new();
    for part in parse_lsblk(&text) {
        match ensure_mounted(&part) {
            Ok(mount) => {
                let windows = mount.join("Windows").is_dir();
                let users = list_users(&mount.join("Users"));
                found.push(Found { part, mount, windows, users });
            }
            Err(e) => eprintln!("  {}: {e}", part.name),
        }
    }
    Ok(found)
}

fn scan() -> Result {
    let found = discover()?;
    if found.is_empty() {
        println!("NTFS bölümü bulunamadı.");
    }
    for f in &found {
        let label = if f.part.label.is_empty() { "-" } else { &f.part.label };
        println!("/dev/{} ({}, {}) -> {}", f.part.name, label, f.part.size, f.mount.display());
        println!("  Windows kurulumu: {}", if f.windows { "var" } else { "yok" });
        if f.windows {
            println!("  Kullanıcılar: {}", if f.users.is_empty() { "-".into() } else { f.users.join(", ") });
        }
    }
    Ok(())
}

fn windows_root() -> Result<PathBuf> {
    let found = discover()?;
    let mut wins = found.into_iter().filter(|f| f.windows);
    let first = wins.next().ok_or("Windows bölümü bulunamadı; önce `osmium-migrate scan` çalıştır")?;
    if wins.next().is_some() {
        println!("Birden fazla Windows bulundu; /dev/{} kullanılıyor.", first.part.name);
    }
    Ok(first.mount)
}

/// `<root>/Users/<user>`; the name must be a single plain path component.
fn user_dir_path(root: &Path, user: &str) -> Result<PathBuf> {
    if user.is_empty() || user == "." || user == ".." || user.contains(['/', '\\']) {
        return Err(format!("geçersiz kullanıcı adı: {user:?}"));
    }
    Ok(root.join("Users").join(user))
}

fn user_home(root: &Path, user: &str) -> Result<PathBuf> {
    let p = user_dir_path(root, user)?;
    if p.is_dir() { Ok(p) } else { Err(format!("{} bulunamadı", p.display())) }
}

// ---------- helpers ----------

fn confirm(question: &str) -> bool {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == "evet"
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

fn target_dir(key: &str, name: &str) -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    // xdg-user-dir prints $HOME when the key is not configured.
    let xdg = output(Command::new("xdg-user-dir").arg(key))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty() && *p != home);
    Ok(xdg.unwrap_or_else(|| home.join(name)))
}

// ---------- copying ----------

#[derive(Default, Debug, PartialEq)]
struct Stats {
    files: u64,
    bytes: u64,
    existing: u64,
    errors: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.files += o.files;
        self.bytes += o.bytes;
        self.existing += o.existing;
        self.errors += o.errors;
    }
}

/// Copies one file; never overwrites (create_new) and removes its own partial output on failure.
fn copy_new(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    let res = fs::File::open(src).and_then(|mut inp| std::io::copy(&mut inp, &mut out));
    if res.is_err() {
        drop(out);
        let _ = fs::remove_file(dst);
    }
    res.map(|_| ())
}

/// Walks `src`; with `dry` only counts. Symlinks and junctions are never followed.
fn copy_tree(src: &Path, dst: &Path, dry: bool, st: &mut Stats) {
    let Ok(rd) = fs::read_dir(src) else {
        st.errors += 1;
        return;
    };
    for e in rd.flatten() {
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let Ok(meta) = fs::symlink_metadata(&s) else {
            st.errors += 1;
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            copy_tree(&s, &d, dry, st);
        } else if meta.is_file() {
            let lower = e.file_name().to_string_lossy().to_lowercase();
            if JUNK_FILES.contains(&lower.as_str()) {
                continue;
            }
            if d.symlink_metadata().is_ok() {
                st.existing += 1;
                continue;
            }
            match if dry { Ok(()) } else { copy_new(&s, &d) } {
                Ok(()) => {
                    st.files += 1;
                    st.bytes += meta.len();
                }
                Err(err) => {
                    eprintln!("  kopyalanamadı {}: {err}", s.display());
                    st.errors += 1;
                }
            }
        }
    }
}

fn report(st: &Stats) {
    println!(
        "{} dosya ({}), {} zaten var (atlandı), {} hata",
        st.files,
        human(st.bytes),
        st.existing,
        st.errors
    );
}

// ---------- steps ----------

fn files(root: &Path, user: &str) -> Result {
    let home = user_home(root, user)?;
    let (mut jobs, mut total) = (Vec::new(), Stats::default());
    for (name, key) in KNOWN_DIRS {
        let src = home.join(name);
        if !src.is_dir() {
            continue;
        }
        let dst = target_dir(key, name)?;
        let mut st = Stats::default();
        copy_tree(&src, &dst, true, &mut st);
        println!(
            "  {name:<10} {} dosya, {}, {} zaten var -> {}",
            st.files,
            human(st.bytes),
            st.existing,
            dst.display()
        );
        total.add(&st);
        jobs.push((src, dst));
    }
    if total.files == 0 {
        println!("Kopyalanacak yeni dosya yok.");
        return Ok(());
    }
    if !confirm(&format!("Toplam {} dosya, {} kopyalanacak (Windows'a dokunulmaz). Devam için 'evet' yaz:", total.files, human(total.bytes))) {
        return Ok(());
    }
    let mut done = Stats::default();
    for (src, dst) in &jobs {
        copy_tree(src, dst, false, &mut done);
    }
    report(&done);
    Ok(())
}

fn wallpaper(root: &Path, user: &str) -> Result {
    let src = user_home(root, user)?.join("AppData/Roaming/Microsoft/Windows/Themes/TranscodedWallpaper");
    if !src.is_file() {
        return Err(format!("{} bulunamadı", src.display()));
    }
    let dst = target_dir("PICTURES", "Pictures")?.join("windows-wallpaper.jpg");
    if dst.symlink_metadata().is_ok() {
        println!("{} zaten var, atlandı.", dst.display());
    } else if confirm(&format!("Duvar kağıdı {} dosyasına kopyalanacak. Devam için 'evet' yaz:", dst.display())) {
        copy_new(&src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        println!("Kopyalandı.");
    } else {
        return Ok(());
    }
    println!(
        "Ayarlamak için (Hyprland ayarına dokunmadım):\n  geçici: swaybg -i {p} -m fill\n  hyprpaper.conf: preload = {p}  ve  wallpaper = ,{p}",
        p = dst.display()
    );
    Ok(())
}

fn fonts(root: &Path, user: &str) -> Result {
    println!("Not: Windows/Fonts (sistem yazı tipleri) lisans nedeniyle kopyalanmaz; yalnızca kullanıcının kendi yazı tipleri alınır.");
    let src = user_home(root, user)?.join("AppData/Local/Microsoft/Windows/Fonts");
    if !src.is_dir() {
        println!("Kullanıcıya ait yazı tipi klasörü yok.");
        return Ok(());
    }
    let dst = data_home()?.join("fonts/windows");
    let mut st = Stats::default();
    copy_tree(&src, &dst, true, &mut st);
    print!("  ");
    report(&st);
    if st.files == 0 || !confirm(&format!("Yazı tipleri {} içine kopyalanacak. Devam için 'evet' yaz:", dst.display())) {
        return Ok(());
    }
    let mut done = Stats::default();
    copy_tree(&src, &dst, false, &mut done);
    report(&done);
    if !succeeds(Command::new("fc-cache").arg("-f")) {
        eprintln!("uyarı: fc-cache çalışmadı; elle çalıştır: fc-cache -f");
    }
    Ok(())
}

// ---------- wifi ----------

#[derive(Debug, PartialEq)]
struct WifiProfile {
    ssid: String,
    auth: String,
}

#[derive(Debug, PartialEq)]
enum Sec {
    Open,
    Key(&'static str),
    Unsupported,
}

/// Text of the first `<name>..</name>`; minimal on purpose, the profile format is flat.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{name}>"))?;
    Some(s[start..start + end].trim())
}

fn unxml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn parse_profile(xml: &str) -> Option<WifiProfile> {
    let xml = xml.trim_start_matches('\u{feff}');
    // The first <name> is the profile name; the SSID has its own inside <SSID>.
    let block = xml.find("<SSID>").map_or(xml, |i| &xml[i..]);
    let ssid = unxml(tag(block, "name").or_else(|| tag(xml, "name"))?);
    if ssid.is_empty() {
        return None;
    }
    Some(WifiProfile { ssid, auth: tag(xml, "authentication").unwrap_or("").to_string() })
}

fn security(auth: &str) -> Sec {
    match auth.to_ascii_lowercase().as_str() {
        "open" => Sec::Open,
        "wpa2psk" | "wpapsk" => Sec::Key("wpa-psk"),
        "wpa3sae" => Sec::Key("sae"),
        _ => Sec::Unsupported,
    }
}

fn read_profiles(base: &Path) -> Vec<WifiProfile> {
    let mut v: Vec<WifiProfile> = Vec::new();
    let Ok(ifaces) = fs::read_dir(base) else { return v };
    for iface in ifaces.flatten() {
        let Ok(rd) = fs::read_dir(iface.path()) else { continue };
        for f in rd.flatten() {
            let p = f.path();
            if p.extension().is_none_or(|e| !e.eq_ignore_ascii_case("xml")) {
                continue;
            }
            let Ok(bytes) = fs::read(&p) else { continue };
            if let Some(prof) = parse_profile(&String::from_utf8_lossy(&bytes))
                && !v.iter().any(|o| o.ssid == prof.ssid)
            {
                v.push(prof);
            }
        }
    }
    v.sort_by(|a, b| a.ssid.cmp(&b.ssid));
    v
}

fn wifi(root: &Path) -> Result {
    let profiles = read_profiles(&root.join("ProgramData/Microsoft/Wlansvc/Profiles/Interfaces"));
    if profiles.is_empty() {
        println!("Kayıtlı Wi-Fi profili bulunamadı.");
        return Ok(());
    }
    let existing: BTreeSet<String> = output(Command::new("nmcli").args(["-t", "-f", "NAME", "connection", "show"]))?
        .lines()
        .map(str::to_string)
        .collect();
    let mut todo = Vec::new();
    for p in &profiles {
        let sec = security(&p.auth);
        let state = if existing.contains(&p.ssid) {
            "zaten var"
        } else if sec == Sec::Unsupported {
            "desteklenmiyor (kurumsal/WEP)"
        } else {
            todo.push((p, sec));
            "eklenecek"
        };
        println!("  {:<30} {:<10} {state}", p.ssid, p.auth);
    }
    if todo.is_empty() {
        return Ok(());
    }
    println!("Şifreler Windows tarafından şifrelendiği için aktarılamaz; ilk bağlanışta şifreyi sen gireceksin.");
    if !confirm(&format!("{} ağ şifresiz olarak NetworkManager'a eklenecek. Devam için 'evet' yaz:", todo.len())) {
        return Ok(());
    }
    for (p, sec) in todo {
        let mut c = Command::new("nmcli");
        c.args(["connection", "add", "type", "wifi", "con-name", p.ssid.as_str(), "ifname", "*", "ssid", p.ssid.as_str()]);
        if let Sec::Key(k) = sec {
            c.args(["wifi-sec.key-mgmt", k]);
        }
        match run(&mut c) {
            Ok(()) => println!("  eklendi: {}", p.ssid),
            Err(e) => eprintln!("  eklenemedi {}: {e}", p.ssid),
        }
    }
    Ok(())
}

// ---------- apps ----------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mapping {
    Native { pkg: &'static str, aur: bool },
    Wine(&'static str),
    /// `pkg` is an official-repo package or "" when there is none to install.
    Alt { name: &'static str, pkg: &'static str },
}

const fn off(pkg: &'static str) -> Mapping {
    Mapping::Native { pkg, aur: false }
}
const fn aur(pkg: &'static str) -> Mapping {
    Mapping::Native { pkg, aur: true }
}
const fn alt(name: &'static str, pkg: &'static str) -> Mapping {
    Mapping::Alt { name, pkg }
}

/// Order matters: first match wins, so specific names go before generic ones.
const APPS: &[(&[&str], Mapping)] = &[
    (&["google chrome"], aur("google-chrome")),
    (&["firefox"], off("firefox")),
    (&["microsoft edge"], aur("microsoft-edge-stable-bin")),
    (&["brave"], aur("brave-bin")),
    (&["opera"], aur("opera")),
    (&["libreoffice"], off("libreoffice-fresh")),
    (&["outlook"], alt("Thunderbird", "thunderbird")),
    (&["microsoft office", "office", "word", "excel", "powerpoint"], alt("LibreOffice", "libreoffice-fresh")),
    (&["photoshop"], alt("GIMP / Krita", "gimp")),
    (&["illustrator"], alt("Inkscape", "inkscape")),
    (&["premiere"], alt("Kdenlive", "kdenlive")),
    (&["after effects"], alt("Blender / Natron", "blender")),
    (&["lightroom"], alt("darktable", "darktable")),
    (&["creative cloud"], Mapping::Wine("Linux sürümü yok; Wine ile sorunlu, tek tek uygulamaların alternatiflerine bak")),
    (&["adobe reader", "acrobat", "foxit"], alt("Okular", "okular")),
    (&["spotify"], off("spotify-launcher")),
    (&["discord"], off("discord")),
    (&["steam"], off("steam")),
    (&["epic games"], alt("Heroic Games Launcher (AUR: heroic-games-launcher-bin)", "")),
    (&["gog galaxy", "battle.net"], alt("Lutris", "lutris")),
    (&["ea app", "origin", "ubisoft connect"], Mapping::Wine("Wine/Proton (Lutris veya Heroic ile)")),
    (&["minecraft"], alt("Prism Launcher", "prismlauncher")),
    (&["visual studio code", "vs code", "vscode"], off("code")),
    (&["visual studio"], Mapping::Wine("Wine ile çalışmaz; JetBrains Rider (AUR: rider) veya VS Code dene")),
    (&["intellij"], off("intellij-idea-community-edition")),
    (&["pycharm"], off("pycharm-community-edition")),
    (&["sublime text"], aur("sublime-text-4")),
    (&["notepad++"], alt("Kate", "kate")),
    (&["7-zip", "7zip"], off("7zip")),
    (&["winrar"], alt("unrar / 7zip", "unrar")),
    (&["vlc"], off("vlc")),
    (&["mpv"], off("mpv")),
    (&["potplayer", "kmplayer", "media player classic", "windows media player"], alt("mpv / VLC", "mpv")),
    (&["obs studio", "obs"], off("obs-studio")),
    (&["zoom"], aur("zoom")),
    (&["microsoft teams", "teams"], aur("teams-for-linux")),
    (&["skype"], aur("skypeforlinux-bin")),
    (&["slack"], aur("slack-desktop")),
    (&["telegram"], off("telegram-desktop")),
    (&["whatsapp"], alt("WhatsApp Web (tarayıcıda web.whatsapp.com)", "")),
    (&["blender"], off("blender")),
    (&["gimp"], off("gimp")),
    (&["krita"], off("krita")),
    (&["inkscape"], off("inkscape")),
    (&["kdenlive"], off("kdenlive")),
    (&["davinci resolve"], aur("davinci-resolve")),
    (&["audacity"], off("audacity")),
    (&["handbrake"], off("handbrake")),
    (&["qbittorrent"], off("qbittorrent")),
    (&["utorrent", "bittorrent"], alt("qBittorrent", "qbittorrent")),
    (&["thunderbird"], off("thunderbird")),
    (&["git"], off("git")),
    (&["python"], off("python")),
    (&["node.js", "nodejs"], off("nodejs")),
    (&["java", "jdk", "jre", "openjdk"], off("jdk-openjdk")),
    (&["docker"], off("docker")),
    (&["putty"], alt("OpenSSH (ssh komutu)", "openssh")),
    (&["filezilla"], off("filezilla")),
    (&["winscp"], alt("FileZilla", "filezilla")),
    (&["keepass"], alt("KeePassXC", "keepassxc")),
    (&["bitwarden"], off("bitwarden")),
    (&["dropbox"], aur("dropbox")),
    (&["google drive"], alt("rclone", "rclone")),
    (&["onedrive"], aur("onedrive-abraunegg")),
    (&["itunes"], alt("Rhythmbox", "rhythmbox")),
    (&["foobar2000"], alt("Strawberry", "strawberry")),
    (&["winamp"], alt("Audacious", "audacious")),
    (&["paint.net"], alt("Pinta", "pinta")),
    (&["unity"], aur("unityhub")),
    (&["unreal engine", "unreal"], aur("unreal-engine")),
    (&["autodesk", "autocad", "3ds max", "maya"], Mapping::Wine("Linux sürümü yok, Wine çoğu zaman çalışmaz; alternatif: Blender / FreeCAD")),
    (&["anydesk"], aur("anydesk-bin")),
    (&["teamviewer"], aur("teamviewer")),
    (&["sharex"], alt("Flameshot", "flameshot")),
    (&["everything"], alt("FSearch (AUR: fsearch)", "")),
    (&["powertoys"], alt("Birebir karşılığı yok (Hyprland + anyrun + flameshot parçaları karşılar)", "")),
    (&["windows terminal"], alt("Alacritty", "alacritty")),
    (&["obsidian"], off("obsidian")),
    (&["postman"], aur("postman-bin")),
    (&["wireshark"], off("wireshark-qt")),
    (&["virtualbox"], off("virtualbox")),
    (&["calibre"], off("calibre")),
    (&["ultimaker cura", "cura"], off("cura")),
    (&["ccleaner"], alt("BleachBit", "bleachbit")),
    (&["winmerge"], alt("Meld", "meld")),
    (&["geforce experience"], alt("nvidia-settings", "nvidia-settings")),
    (&["rufus", "balenaetcher", "etcher"], alt("GNOME Disks / dd / Ventoy", "gnome-disk-utility")),
    (&["fl studio", "ableton"], Mapping::Wine("Wine/Proton ile çalışabilir; alternatif: Ardour / LMMS")),
];

fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = hay[i + needle.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before && after
    })
}

fn lookup(name: &str) -> Option<Mapping> {
    let n = name.to_lowercase();
    APPS.iter().find(|(keys, _)| keys.iter().any(|k| contains_word(&n, k))).map(|(_, m)| *m)
}

fn collect_lnk(dir: &Path, depth: u32, out: &mut BTreeMap<String, String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if meta.is_dir() && depth > 0 {
            collect_lnk(&e.path(), depth - 1, out);
        } else if meta.is_file()
            && let Some(stem) = name.strip_suffix(".lnk").or_else(|| name.strip_suffix(".LNK"))
        {
            add_app(stem, out);
        }
    }
}

fn add_app(name: &str, out: &mut BTreeMap<String, String>) {
    let lower = name.to_lowercase();
    if !IGNORED_APPS.contains(&lower.as_str()) && !lower.starts_with("uninstall") {
        out.entry(lower).or_insert_with(|| name.to_string());
    }
}

fn installed_apps(root: &Path) -> Vec<String> {
    let mut map = BTreeMap::new();
    for d in ["Program Files", "Program Files (x86)"] {
        if let Ok(rd) = fs::read_dir(root.join(d)) {
            for e in rd.flatten().filter(|e| e.path().is_dir()) {
                add_app(&e.file_name().to_string_lossy(), &mut map);
            }
        }
    }
    collect_lnk(&root.join("ProgramData/Microsoft/Windows/Start Menu/Programs"), 3, &mut map);
    map.into_values().collect()
}

fn describe(m: Option<Mapping>) -> (String, String) {
    match m {
        Some(Mapping::Native { pkg, aur }) => ("Linux sürümü var".into(), if aur { format!("{pkg} (AUR)") } else { pkg.into() }),
        Some(Mapping::Wine(note)) => ("Wine/Proton ile çalışabilir".into(), note.into()),
        Some(Mapping::Alt { name, pkg }) => (format!("Alternatif: {name}"), pkg.into()),
        None => ("bilinmiyor".into(), String::new()),
    }
}

/// Official-repo packages worth installing: native ones plus alternatives with a package.
fn official_pkgs(maps: &[Option<Mapping>]) -> (BTreeSet<&'static str>, BTreeSet<&'static str>) {
    let (mut official, mut aurs) = (BTreeSet::new(), BTreeSet::new());
    for m in maps.iter().flatten() {
        match *m {
            Mapping::Native { pkg, aur: true } => aurs.insert(pkg),
            Mapping::Native { pkg, .. } | Mapping::Alt { pkg, .. } if !pkg.is_empty() => official.insert(pkg),
            _ => false,
        };
    }
    (official, aurs)
}

fn apps(root: &Path, install: bool) -> Result {
    let names = installed_apps(root);
    if names.is_empty() {
        println!("Kurulu program bulunamadı.");
        return Ok(());
    }
    let maps: Vec<Option<Mapping>> = names.iter().map(|n| lookup(n)).collect();
    println!("{:<34} {:<52} Ayrıntı", "Program", "Durum");
    for (n, m) in names.iter().zip(&maps) {
        let (status, detail) = describe(*m);
        println!("{n:<34} {status:<52} {detail}");
    }
    let known = maps.iter().flatten().count();
    println!("\n{} program, {} eşleşti, {} bilinmiyor.", names.len(), known, names.len() - known);
    if install {
        let (official, aurs) = official_pkgs(&maps);
        if !official.is_empty() {
            println!("\nResmi depodan kurmak için (çalıştırılmadı):\n  sudo pacman -S --needed {}", official.iter().copied().collect::<Vec<_>>().join(" "));
        }
        if !aurs.is_empty() {
            println!("AUR (elle, bir AUR yardımcısıyla): {}", aurs.iter().copied().collect::<Vec<_>>().join(" "));
        }
    } else {
        println!("Kurulum komutu için: osmium-migrate apps --install");
    }
    Ok(())
}

// ---------- main ----------

fn step(name: &str, res: Result) {
    println!("\n== {name} ==");
    if let Err(e) = res {
        eprintln!("osmium-migrate: {name} atlandı: {e}");
    }
}

fn all(root: &Path, user: &str) {
    step("files", files(root, user));
    step("wallpaper", wallpaper(root, user));
    step("fonts", fonts(root, user));
    step("wifi", wifi(root));
    step("apps", apps(root, false));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match args[..] {
        ["scan"] => scan(),
        ["files", u] => windows_root().and_then(|r| files(&r, u)),
        ["wallpaper", u] => windows_root().and_then(|r| wallpaper(&r, u)),
        ["fonts", u] => windows_root().and_then(|r| fonts(&r, u)),
        ["wifi"] => windows_root().and_then(|r| wifi(&r)),
        ["apps"] => windows_root().and_then(|r| apps(&r, false)),
        ["apps", "--install"] => windows_root().and_then(|r| apps(&r, true)),
        ["all", u] => windows_root().map(|r| all(&r, u)),
        _ => Err("kullanım: osmium-migrate scan | files <kullanıcı> | wallpaper <kullanıcı> | fonts <kullanıcı> | wifi | apps [--install] | all <kullanıcı>".into()),
    };
    exit_on_err("osmium-migrate", res);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = "sda disk  500G \nsda1 vfat ESP 512M /boot\nsda2 ntfs Windows\\x20Disk 200G \nnvme0n1p3 ntfs  1T /run/media/x/Data\\x20Disk\nsdb1 ext4 root 10G /\n";

    #[test]
    fn lsblk_keeps_only_ntfs_and_unescapes() {
        let p = parse_lsblk(LSBLK);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], Part { name: "sda2".into(), fstype: "ntfs".into(), label: "Windows Disk".into(), size: "200G".into(), mount: String::new() });
        assert_eq!((p[1].label.as_str(), p[1].mount.as_str()), ("", "/run/media/x/Data Disk"));
    }

    const WLAN: &str = "\u{feff}<?xml version=\"1.0\"?><WLANProfile><name>Home Profile</name><SSIDConfig><SSID><hex>41</hex><name>Caf&amp;e Net</name></SSID></SSIDConfig><MSM><security><authEncryption><authentication>WPA2PSK</authentication><encryption>AES</encryption></authEncryption><sharedKey><protected>true</protected><keyMaterial>01000000D0</keyMaterial></sharedKey></security></MSM></WLANProfile>";

    #[test]
    fn wifi_xml_parsing() {
        let p = parse_profile(WLAN).unwrap();
        assert_eq!(p, WifiProfile { ssid: "Caf&e Net".into(), auth: "WPA2PSK".into() });
        assert!(parse_profile("<x/>").is_none());
        assert_eq!(security("WPA2PSK"), Sec::Key("wpa-psk"));
        assert_eq!(security("WPA3SAE"), Sec::Key("sae"));
        assert_eq!(security("open"), Sec::Open);
        assert_eq!(security("WPA2"), Sec::Unsupported);
    }

    #[test]
    fn app_matching_is_case_insensitive_and_word_based() {
        assert_eq!(lookup("Mozilla Firefox"), Some(off("firefox")));
        assert_eq!(lookup("Google Chrome"), Some(aur("google-chrome")));
        assert_eq!(lookup("MICROSOFT OFFICE"), Some(alt("LibreOffice", "libreoffice-fresh")));
        assert_eq!(lookup("Visual Studio Code"), Some(off("code")));
        assert_eq!(lookup("Git"), Some(off("git")));
        assert_eq!(lookup("Digital Editions"), None);
        assert_eq!(lookup("Notepad++"), Some(alt("Kate", "kate")));
    }

    #[test]
    fn install_list_splits_official_and_aur() {
        let (o, a) = official_pkgs(&[lookup("Firefox"), lookup("Google Chrome"), lookup("Word"), lookup("WhatsApp"), None]);
        assert_eq!(o.into_iter().collect::<Vec<_>>(), ["firefox", "libreoffice-fresh"]);
        assert_eq!(a.into_iter().collect::<Vec<_>>(), ["google-chrome"]);
    }

    #[test]
    fn windows_user_paths() {
        let r = Path::new("/mnt/win");
        assert_eq!(user_dir_path(r, "Ali").unwrap(), r.join("Users").join("Ali"));
        for bad in ["", "..", ".", "a/b", "a\\b"] {
            assert!(user_dir_path(r, bad).is_err(), "{bad}");
        }
        assert!(is_system_user("Default User") && is_system_user("PUBLIC") && !is_system_user("Ali"));
    }

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("osmium-migrate-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn copy_skips_existing_and_junk() {
        let t = tmp("copy");
        let (src, dst) = (t.join("src"), t.join("dst"));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), "new").unwrap();
        fs::write(src.join("sub/b.txt"), "bb").unwrap();
        fs::write(src.join("desktop.ini"), "x").unwrap();
        fs::write(dst.join("a.txt"), "old").unwrap();

        let mut dry = Stats::default();
        copy_tree(&src, &dst, true, &mut dry);
        assert_eq!(dry, Stats { files: 1, bytes: 2, existing: 1, errors: 0 });
        assert!(!dst.join("sub").exists(), "dry run must not write");

        let mut real = Stats::default();
        copy_tree(&src, &dst, false, &mut real);
        assert_eq!(real, dry);
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "bb");
        assert!(!dst.join("desktop.ini").exists());

        let mut again = Stats::default();
        copy_tree(&src, &dst, false, &mut again);
        assert_eq!(again, Stats { files: 0, bytes: 0, existing: 2, errors: 0 });
        fs::remove_dir_all(&t).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_does_not_follow_symlinks() {
        let t = tmp("link");
        let (src, dst) = (t.join("src"), t.join("dst"));
        fs::create_dir_all(t.join("outside")).unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(t.join("outside/secret"), "s").unwrap();
        fs::write(src.join("real.txt"), "r").unwrap();
        std::os::unix::fs::symlink(t.join("outside"), src.join("junction")).unwrap();
        std::os::unix::fs::symlink(t.join("outside/secret"), src.join("file-link")).unwrap();

        let mut st = Stats::default();
        copy_tree(&src, &dst, false, &mut st);
        assert_eq!(st.files, 1);
        assert!(dst.join("real.txt").exists());
        assert!(!dst.join("junction").exists() && !dst.join("file-link").exists());
        fs::remove_dir_all(&t).unwrap();
    }
}
