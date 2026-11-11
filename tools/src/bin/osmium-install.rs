//! OsmiumOS installer backend, driven by the Quickshell installer UI.
//!
//!   osmium-install disks        JSON array of installable disks
//!   osmium-install check        validate the config on stdin, print {"ok":..}
//!   osmium-install run          install; JSON progress lines on stdout
//!
//! Config is `key=value` lines on stdin (passwords never on argv). Disk layout and boot setup:
//! GPT, ESP at /efi, btrfs @ @home @cache @log @snapshots with @ as the default subvolume
//! (so `snapper rollback` works), optional LUKS2, systemd-boot with a signed UKI (see
//! osmium-boot). Built for speed: the squashfs is unpacked on all cores, independent chroot
//! work runs in parallel, the initramfs is zstd -1, and there is no swap file.
//!
//! The initramfs is systemd-based: the LUKS passphrase is read with the console keymap
//! (sd-vconsole), and a TPM2 token is used when osmium-boot enrolled one on the first boot.
//! For that the installer leaves a one-time key file in /var/lib/osmium/boot/tpm.key.

use osmium_tools::{Result, json_str, output, succeeds};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

const MNT: &str = "/mnt";
const MAPPER: &str = "cryptroot";
/// One-time LUKS key for the first-boot TPM2 enrollment (osmium-boot ilk), on the encrypted root.
const TPM_KEY: &str = "/var/lib/osmium/boot/tpm.key";
const TPM_KEY_TMP: &str = "/run/osmium-install-tpm.key";
const ESP_MOUNT: &str = "/efi";
const BOOTMNT: &str = "/run/archiso/bootmnt";
const LIVE_ROOT: &str = "/run/archiso/airootfs";
const BTRFS_OPTS: &str = "noatime,compress=zstd:1";
/// (subvolume, mount point). `/` is not mounted by name: it is the default subvolume.
const SUBVOLS: &[(&str, &str)] =
    &[("@", "/"), ("@home", "/home"), ("@cache", "/var/cache"), ("@log", "/var/log"), ("@snapshots", "/.snapshots")];
/// Groups of the new user; groups missing from the image are skipped.
const GROUPS: &[&str] = &["users", "wheel", "video", "input", "audio", "lp", "network", "storage", "i2c"];
/// Units to enable or disable on the installed system: (unit, enable).
const SERVICES: &[(&str, bool)] = &[
    ("NetworkManager.service", true),
    ("snapper-timeline.timer", true),
    ("snapper-cleanup.timer", true),
    // Boot counting and rollback, first-boot TPM2 enrollment, systemd-boot updates.
    ("osmium-boot-check.service", true),
    ("osmium-boot-ok.service", true),
    ("osmium-boot-first.service", true),
    ("systemd-boot-update.service", true),
    ("power-profiles-daemon.service", true),
    ("iwd.service", false),
    ("systemd-networkd.service", false),
    ("systemd-networkd.socket", false),
    ("systemd-networkd-wait-online.service", false),
    ("sshd.service", false),
];

/// (step id, pct where the step starts, Turkish message). Expected wall time on NVMe/8 cores:
/// disk 1s, format 2s (LUKS +3s), copy 20-40s, config 6s, boot 8s, finish 2s.
const PLAN: &[(&str, u8, &str)] = &[
    ("disk", 0, "Disk hazırlanıyor"),
    ("format", 4, "Dosya sistemleri oluşturuluyor"),
    ("copy", 9, "Sistem dosyaları kopyalanıyor"),
    ("config", 72, "Sistem yapılandırılıyor"),
    ("boot", 84, "Önyükleyici ve initramfs hazırlanıyor"),
    ("finish", 97, "Son işlemler yapılıyor"),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let code = match args.next().as_deref() {
        Some("disks") => cmd_disks(),
        Some("firmware") => cmd_firmware(),
        Some("check") => cmd_check(),
        Some("run") => cmd_run(),
        _ => {
            eprintln!("usage: osmium-install disks|check|run  (config on stdin for check/run)");
            2
        }
    };
    std::process::exit(code);
}

fn cmd_disks() -> i32 {
    match list_disks() {
        Ok(d) => {
            println!("[{}]", d.iter().map(Disk::json).collect::<Vec<_>>().join(","));
            0
        }
        Err(e) => {
            eprintln!("osmium-install: {e}");
            1
        }
    }
}

/// `{"secure_boot":..,"setup_mode":..,"tpm":..}` for the installer's encryption notes.
fn cmd_firmware() -> i32 {
    let var = |name: &str| fs::read(format!("/sys/firmware/efi/efivars/{name}-8be4df61-93ca-11d2-aa0d-00e098032b8c")).ok();
    let on = |name: &str| var(name).is_some_and(|b| b.get(4) == Some(&1));
    println!("{}", firmware_json(on("SecureBoot"), on("SetupMode"), Path::new("/sys/class/tpm/tpm0").exists()));
    0
}

fn firmware_json(secure_boot: bool, setup_mode: bool, tpm: bool) -> String {
    format!("{{\"secure_boot\":{secure_boot},\"setup_mode\":{setup_mode},\"tpm\":{tpm}}}")
}

fn read_config() -> Config {
    let mut s = String::new();
    let _ = io::stdin().read_to_string(&mut s);
    parse_config(&s)
}

fn cmd_check() -> i32 {
    match check(&read_config()) {
        Ok(()) => println!("{{\"ok\":true}}"),
        Err(e) => println!("{{\"ok\":false,\"error\":{}}}", json_str(&e)),
    }
    0
}

fn cmd_run() -> i32 {
    let t0 = Instant::now();
    let cfg = read_config();
    if let Err(e) = check(&cfg) {
        return fail("check", &e);
    }
    if output(Command::new("id").arg("-u")).map(|s| s.trim() != "0").unwrap_or(true) {
        return fail("check", "Kurulum için yönetici (root) yetkisi gerekir.");
    }
    let res = install(&cfg);
    cleanup();
    match res {
        Ok(()) => {
            emit_line(&format!("{{\"done\":true,\"seconds\":{}}}", t0.elapsed().as_secs()));
            0
        }
        Err((step, e)) => fail(step, &e),
    }
}

fn fail(step: &str, msg: &str) -> i32 {
    emit_line(&format!("{{\"error\":{},\"step\":{}}}", json_str(msg), json_str(step)));
    1
}

// ---------------------------------------------------------------- pure logic

#[derive(Debug, Clone, PartialEq)]
struct Disk {
    path: String,
    model: String,
    size: u64,
    removable: bool,
    transport: String,
    /// Set by `probe_windows` (root only).
    windows: Option<Windows>,
}

impl Disk {
    fn json(&self) -> String {
        format!(
            "{{\"path\":{},\"model\":{},\"size\":{},\"removable\":{},\"transport\":{},\"windows\":{}}}",
            json_str(&self.path),
            json_str(&self.model),
            self.size,
            self.removable,
            json_str(&self.transport),
            self.windows.as_ref().map_or("null".into(), Windows::json)
        )
    }
}

/// Decodes the `\xNN` escapes lsblk -P uses.
fn unescape_lsblk(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'x')
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

/// Parses one `lsblk -P` line: `KEY="value" KEY2="value"`.
fn parse_pairs(line: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut rest = line.trim();
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim().to_string();
        let after = &rest[eq + 2..];
        let Some(end) = after.find('"') else { break };
        m.insert(key, unescape_lsblk(&after[..end]));
        rest = after[end + 1..].trim_start();
    }
    m
}

/// Installable disks from `lsblk -d -n -b -P` output; `live` are the disks holding the live medium.
fn filter_disks(lsblk: &str, live: &[String]) -> Vec<Disk> {
    lsblk
        .lines()
        .map(parse_pairs)
        .filter(|m| m.get("TYPE").is_some_and(|t| t == "disk") && m.get("RO").is_none_or(|r| r == "0"))
        .filter_map(|m| {
            let path = m.get("PATH")?.clone();
            let size = m.get("SIZE")?.parse::<u64>().ok().filter(|&s| s > 0)?;
            let virt = ["/dev/zram", "/dev/loop", "/dev/ram", "/dev/sr", "/dev/fd"];
            if virt.iter().any(|p| path.starts_with(p)) || live.contains(&path) {
                return None;
            }
            Some(Disk {
                model: m.get("MODEL").map(|s| s.trim().to_string()).unwrap_or_default(),
                size,
                removable: m.get("RM").is_some_and(|r| r == "1"),
                transport: m.get("TRAN").cloned().unwrap_or_default(),
                windows: None,
                path,
            })
        })
        .collect()
}

#[derive(Debug, Default, Clone)]
struct Config {
    disk: String,
    encrypt: bool,
    passphrase: String,
    hostname: String,
    username: String,
    fullname: String,
    password: String,
    timezone: String,
    locale: String,
    keymap: String,
    xkblayout: String,
    dots: String,
    dots_mode: String,
    /// Install next to Windows instead of wiping the disk.
    alongside: bool,
    /// Space for OsmiumOS next to Windows, in GiB.
    size_gb: u64,
}

/// `key=value` lines. Values are verbatim (a password may hold `=` or spaces); unknown keys ignored.
fn parse_config(input: &str) -> Config {
    let mut c = Config {
        timezone: "UTC".into(),
        locale: "en_US.UTF-8".into(),
        keymap: "us".into(),
        xkblayout: "us".into(),
        dots: "ii".into(),
        dots_mode: "config".into(),
        ..Config::default()
    };
    for line in input.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.to_string();
        match k.trim() {
            "disk" => c.disk = v,
            "encrypt" => c.encrypt = matches!(v.trim(), "1" | "true" | "yes"),
            "passphrase" => c.passphrase = v,
            "hostname" => c.hostname = v,
            "username" => c.username = v,
            "fullname" => c.fullname = v,
            "password" => c.password = v,
            "timezone" if !v.is_empty() => c.timezone = v,
            "locale" if !v.is_empty() => c.locale = v,
            "keymap" if !v.is_empty() => c.keymap = v,
            "xkblayout" if !v.is_empty() => c.xkblayout = v,
            "dots" if !v.is_empty() => c.dots = v,
            "dots_mode" if !v.is_empty() => c.dots_mode = v,
            "mode" => c.alongside = matches!(v.trim(), "yanina" | "1"),
            "size_gb" => c.size_gb = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    c
}

fn valid_username(u: &str) -> bool {
    let b = u.as_bytes();
    (1..=32).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b[1..].iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn valid_hostname(h: &str) -> bool {
    (1..=63).contains(&h.len())
        && !h.starts_with('-')
        && !h.ends_with('-')
        && h.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// `ll_CC.UTF-8`, e.g. `tr_TR.UTF-8`.
fn valid_locale(l: &str) -> bool {
    let Some((name, _)) = l.strip_suffix(".UTF-8").map(|n| (n, ())) else { return false };
    let Some((lang, cc)) = name.split_once('_') else { return false };
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|c| c.is_ascii_lowercase())
        && cc.len() == 2
        && cc.bytes().all(|c| c.is_ascii_uppercase())
}

fn all_chars(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// `ii|waffle|m3ww|synthwave|github:<owner>/<repo>`.
fn valid_dots(d: &str) -> bool {
    if ["ii", "waffle", "m3ww", "synthwave"].contains(&d) {
        return true;
    }
    d.strip_prefix("github:")
        .and_then(|r| r.split_once('/'))
        .is_some_and(|(o, r)| all_chars(o, "-_.") && all_chars(r, "-_.") && !o.starts_with('.') && !r.starts_with('.'))
}

/// Everything that needs no disk access. `tz_exists` looks under /usr/share/zoneinfo.
fn validate(c: &Config, tz_exists: &dyn Fn(&str) -> bool) -> Result {
    if !valid_username(&c.username) || ["root", "osmium"].contains(&c.username.as_str()) {
        return Err("Geçersiz kullanıcı adı: küçük harfle başlamalı; yalnızca a-z, 0-9, _ ve - içermeli (en çok 32 karakter). root ve osmium kullanılamaz.".into());
    }
    if !valid_hostname(&c.hostname) || c.hostname == "localhost" {
        return Err("Geçersiz bilgisayar adı: harf, rakam ve - kullanın (en çok 63 karakter).".into());
    }
    if c.fullname.len() > 100 || c.fullname.chars().any(|ch| ch == ':' || ch.is_control()) {
        return Err("Tam ad ':' veya denetim karakteri içeremez.".into());
    }
    if c.password.is_empty() {
        return Err("Parola boş olamaz.".into());
    }
    if c.encrypt && c.passphrase.is_empty() {
        return Err("Disk şifreleme parolası boş olamaz.".into());
    }
    if !all_chars(&c.timezone, "/_+-") || c.timezone.contains("..") || !tz_exists(&c.timezone) {
        return Err(format!("Geçersiz saat dilimi: {}", c.timezone));
    }
    if !valid_locale(&c.locale) {
        return Err(format!("Geçersiz dil/yerel ayar: {} (örnek: tr_TR.UTF-8)", c.locale));
    }
    if !all_chars(&c.keymap, "._-") {
        return Err(format!("Geçersiz konsol klavye düzeni: {}", c.keymap));
    }
    if !all_chars(&c.xkblayout, "_,()+-") {
        return Err(format!("Geçersiz klavye düzeni: {}", c.xkblayout));
    }
    if !valid_dots(&c.dots) {
        return Err(format!("Geçersiz masaüstü seçimi: {}", c.dots));
    }
    if !["config", "script"].contains(&c.dots_mode.as_str()) {
        return Err("dots_mode 'config' veya 'script' olmalı.".into());
    }
    if !c.disk.starts_with("/dev/") || c.disk.contains("..") {
        return Err("Geçersiz disk yolu.".into());
    }
    if c.alongside && c.size_gb < MIN_GB {
        return Err(format!("Windows'un yanına kurulum için en az {MIN_GB} GB ayırın."));
    }
    Ok(())
}

/// `/dev/sda` -> `/dev/sda1`, `/dev/nvme0n1` -> `/dev/nvme0n1p1`.
fn part_path(disk: &str, n: u8) -> String {
    let sep = if disk.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    format!("{disk}{sep}{n}")
}

fn fstab(root_uuid: &str, esp_uuid: &str, xbootldr_uuid: Option<&str>) -> String {
    let mut s = String::from("# <device> <dir> <type> <options> <dump> <pass>\n");
    // `/` is whatever the default subvolume is: after `snapper rollback` that is the snapshot.
    let _ = writeln!(s, "UUID={root_uuid} / btrfs {BTRFS_OPTS} 0 0");
    for (sub, mp) in SUBVOLS.iter().filter(|(_, mp)| *mp != "/") {
        let _ = writeln!(s, "UUID={root_uuid} {mp} btrfs subvol=/{sub},{BTRFS_OPTS} 0 0");
    }
    let _ = writeln!(s, "UUID={esp_uuid} {ESP_MOUNT} vfat umask=0077 0 2");
    if let Some(x) = xbootldr_uuid {
        let _ = writeln!(s, "UUID={x} /boot vfat umask=0077 0 2");
    }
    s
}

fn hosts(host: &str) -> String {
    format!("127.0.0.1 localhost\n::1 localhost\n127.0.1.1 {host}.localdomain {host}\n")
}

/// locale.gen with only what is needed.
fn locale_gen(locale: &str) -> String {
    let mut s = String::from("en_US.UTF-8 UTF-8\n");
    if locale != "en_US.UTF-8" {
        let _ = writeln!(s, "{locale} UTF-8");
    }
    s
}

fn xorg_keyboard(layout: &str) -> String {
    format!("Section \"InputClass\"\n    Identifier \"system-keyboard\"\n    MatchIsKeyboard \"on\"\n    Option \"XkbLayout\" \"{layout}\"\nEndSection\n")
}

/// mkinitcpio drop-in (compression is set by osmium-postinstall). systemd-based, so the
/// passphrase prompt uses the console keymap and TPM2 tokens work. vfat is built in because
/// osmium-boot's rescue copy boots an older kernel whose modules the new root no longer has,
/// and it still has to mount the ESP.
fn mkinitcpio_dropin(encrypted: bool) -> String {
    let mut hooks = vec!["base", "systemd", "autodetect", "microcode", "modconf", "kms", "keyboard", "sd-vconsole", "block"];
    if encrypted {
        hooks.push("sd-encrypt");
    }
    // osmium-kurtar: the rescue menu (QR code, previous kernel) when the root never appears.
    hooks.extend(["filesystems", "fsck", "osmium-kurtar"]);
    format!("MODULES=(vfat nls_cp437 nls_iso8859_1)\nHOOKS=({})\n", hooks.join(" "))
}

/// /etc/kernel/cmdline, embedded in the UKI. No `rootflags=subvol`: the root is the default
/// subvolume. `panic=10` reboots a kernel panic, so systemd-boot counts the failed try.
fn kernel_cmdline(root_uuid: &str, luks_uuid: Option<&str>) -> String {
    let root = match luks_uuid {
        Some(l) => format!("rd.luks.name={l}={MAPPER} root=/dev/mapper/{MAPPER}"),
        None => format!("root=UUID={root_uuid}"),
    };
    format!("{root} rw quiet panic=10\n")
}

/// systemd-boot menu: hidden unless a key is held; no editor, which would allow changing
/// the kernel command line (and with it the TPM-unlocked boot) without Secure Boot.
const LOADER_CONF: &str = "timeout 0\neditor no\nconsole-mode keep\n# Skips an entry whose tries are used up (systemd >= 258).\npreferred osmium.efi\n";

/// Wanted groups that exist in the image's /etc/group.
fn filter_groups(group_file: &str) -> Vec<&'static str> {
    GROUPS
        .iter()
        .copied()
        .filter(|g| group_file.lines().any(|l| l.split(':').next() == Some(*g)))
        .collect()
}

/// A bare percentage line from `unsquashfs -percentage`.
fn parse_pct(line: &str) -> Option<u8> {
    let t = line.trim().trim_end_matches('%');
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse::<u8>().ok().filter(|&p| p <= 100)
}

/// One line of a Hyprland config with its `kb_layout` value replaced, or None when the line
/// does not set it. Lua lines are only touched when the value is a string literal.
fn kb_layout_line(line: &str, layout: &str, lua: bool) -> Option<String> {
    let body = line.trim_start();
    if body.starts_with('#') || body.starts_with("--") {
        return None;
    }
    let key = line.find("kb_layout")?;
    if line[..key].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let value = line[key + "kb_layout".len()..].trim_start().strip_prefix('=')?;
    let head = &line[..line.len() - value.len()];
    let v = value.trim_start();
    let pad = &value[..value.len() - v.len()];
    if lua {
        let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let end = v[1..].find(q)? + 1;
        Some(format!("{head}{pad}{q}{layout}{q}{}", &v[end + 1..]))
    } else {
        // hyprlang: the value runs to the end of the line.
        let eol = &v[v.trim_end_matches(['\r', '\n']).len()..];
        Some(format!("{head}{pad}{layout}{eol}"))
    }
}

/// A Hyprland config (Lua or hyprlang) with every `kb_layout` set to `layout`; None when
/// nothing changes.
fn set_kb_layout(text: &str, layout: &str, lua: bool) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len() + 8);
    for line in text.split_inclusive('\n') {
        match kb_layout_line(line, layout, lua) {
            Some(new) => {
                changed |= new != line;
                out.push_str(&new);
            }
            None => out.push_str(line),
        }
    }
    changed.then_some(out)
}

fn plan_step(id: &str) -> (u8, &'static str) {
    PLAN.iter().find(|s| s.0 == id).map(|s| (s.1, s.2)).unwrap_or((0, ""))
}

/// Copy phase spans the plan from `copy` to `config`.
fn copy_pct(p: u8) -> u8 {
    let (a, b) = (plan_step("copy").0 as u32, plan_step("config").0 as u32);
    (a + (b - a) * p.min(100) as u32 / 100) as u8
}

// ---------------------------------------------------------------- system access

fn emit_line(s: &str) {
    let mut o = io::stdout().lock();
    let _ = writeln!(o, "{s}");
    let _ = o.flush();
}

fn emit(step: &str, pct: u8, msg: &str) {
    emit_line(&format!("{{\"step\":{},\"pct\":{pct},\"msg\":{}}}", json_str(step), json_str(msg)));
}

fn begin(id: &str) {
    let (pct, msg) = plan_step(id);
    emit(id, pct, msg);
}

fn tail(s: &str, n: usize) -> String {
    let t: Vec<char> = s.trim().chars().rev().take(n).collect();
    t.into_iter().rev().collect()
}

fn finish(what: &str, r: io::Result<Output>) -> Result {
    let o = r.map_err(|e| format!("{what}: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    let out = String::from_utf8_lossy(&o.stdout);
    let detail = if err.trim().is_empty() { out } else { err };
    Err(format!("{what} başarısız ({}): {}", o.status, tail(&detail, 400)))
}

/// Runs a command, capturing output; the error carries the stderr tail.
fn sh(what: &str, c: &mut Command) -> Result {
    finish(what, c.stdin(Stdio::null()).output())
}

/// Like `sh`, feeding `input` on stdin (secrets never go on argv).
fn sh_in(what: &str, c: &mut Command, input: &str) -> Result {
    let mut ch = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: {e}"))?;
    if let Some(mut si) = ch.stdin.take() {
        let _ = si.write_all(input.as_bytes());
    }
    finish(what, ch.wait_with_output())
}

fn ch(prog: &str, args: &[&str]) -> Command {
    let mut c = Command::new("chroot");
    c.arg(MNT)
        .arg(prog)
        .args(args)
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
        .env("HOME", "/root")
        .env("LC_ALL", "C");
    c
}

fn join<T>(h: std::thread::ScopedJoinHandle<'_, std::result::Result<T, String>>) -> std::result::Result<T, String> {
    h.join().map_err(|_| "iç hata: iş parçacığı çöktü".to_string())?
}

fn list_disks() -> Result<Vec<Disk>> {
    let out = output(Command::new("lsblk").args(["-d", "-n", "-b", "-P", "-o", "PATH,SIZE,MODEL,RM,TRAN,TYPE,RO"]))?;
    let mut disks = filter_disks(&out, &live_disks());
    // Reading NTFS and mounting the ESP need root; the UI asks through `sudo -n`.
    if is_root() {
        for d in &mut disks {
            d.windows = probe_windows(&d.path);
        }
    }
    Ok(disks)
}

fn is_root() -> bool {
    output(Command::new("id").arg("-u")).is_ok_and(|s| s.trim() == "0")
}

/// Disks holding the live medium (may be several, e.g. through device-mapper).
fn live_disks() -> Vec<String> {
    let Ok(src) = output(Command::new("findmnt").args(["-n", "-o", "SOURCE", BOOTMNT])) else { return vec![] };
    let src = src.trim().to_string();
    if src.is_empty() {
        return vec![];
    }
    let parents = output(Command::new("lsblk").args(["-n", "-s", "-r", "-o", "PATH,TYPE", &src])).unwrap_or_default();
    let mut v: Vec<String> = parents
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, t)| *t == "disk")
        .map(|(p, _)| p.to_string())
        .collect();
    if v.is_empty() {
        v.push(src);
    }
    v
}

fn check(c: &Config) -> Result {
    validate(c, &|tz| Path::new("/usr/share/zoneinfo").join(tz).is_file())?;
    if !Path::new("/sys/firmware/efi").exists() {
        return Err("Bu bilgisayar UEFI ile açılmamış. Kurulum yalnızca UEFI modunu destekler; ürün yazılımı ayarlarından UEFI ile başlatın.".into());
    }
    let lang = c.locale.split('.').next().unwrap_or_default();
    if !Path::new("/usr/share/i18n/locales").join(lang).is_file() {
        return Err(format!("Bilinmeyen yerel ayar: {}", c.locale));
    }
    let disks = list_disks().map_err(|e| format!("Diskler listelenemedi: {e}"))?;
    let Some(disk) = disks.iter().find(|d| d.path == c.disk) else {
        return Err("Seçilen disk bulunamadı ya da canlı ortamın diski.".into());
    };
    if c.alongside && is_root() {
        alongside_plan(disk.windows.as_ref(), c.size_gb)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- install

type StepErr = (&'static str, String);

fn at(step: &'static str) -> impl Fn(String) -> StepErr {
    move |e| (step, e)
}

/// Where the install goes: wipe mode makes all three, next to Windows the ESP is Windows' own.
struct Target {
    esp: String,
    format_esp: bool,
    /// Our own boot partition (XBOOTLDR, /boot) when the ESP is Windows' small one.
    xbootldr: Option<String>,
    root: String,
}

struct Layout {
    xbootldr: Option<(String, String)>,
    esp: String,
    root_uuid: String,
    esp_uuid: String,
    luks_uuid: Option<String>,
}

fn install(c: &Config) -> std::result::Result<(), StepErr> {
    // A stale /mnt from an earlier attempt would corrupt the new install.
    cleanup();

    begin("disk");
    let target = if c.alongside { prepare_alongside(c) } else { prepare_disk(c) }.map_err(at("disk"))?;

    begin("format");
    let lay = format_and_mount(c, &target).map_err(at("format"))?;

    begin("copy");
    copy_system().map_err(at("copy"))?;
    mount_xbootldr(&lay).map_err(at("copy"))?;

    begin("config");
    configure(c, &lay).map_err(at("config"))?;

    begin("boot");
    boot(c, &lay)?;

    begin("finish");
    Ok(())
}

fn prepare_disk(c: &Config) -> Result<Target> {
    // Unmount/close everything on the disk, deepest device first.
    let tree = output(Command::new("lsblk").args(["-n", "-l", "-o", "PATH,TYPE", &c.disk])).unwrap_or_default();
    let devs: Vec<(String, String)> = tree
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(p, t)| (p.to_string(), t.trim().to_string())))
        .collect();
    for (p, t) in devs.iter().rev() {
        succeeds(Command::new("umount").args(["-A", p]));
        succeeds(Command::new("swapoff").arg(p));
        if t == "crypt" {
            // By name: a /dev/mapper path is not accepted by every cryptsetup version.
            succeeds(Command::new("cryptsetup").args(["close", p.rsplit('/').next().unwrap_or(p)]));
        }
    }
    for (p, t) in devs.iter().rev() {
        if t == "part" || t == "disk" {
            succeeds(Command::new("wipefs").args(["-a", "-f", p]));
        }
    }
    sh("sgdisk", Command::new("sgdisk").args(["--zap-all", &c.disk]))?;
    let root_type = if c.encrypt { "8309" } else { "8300" };
    sh(
        "sgdisk",
        Command::new("sgdisk").args([
            "-n1:0:+1GiB",
            "-t1:ef00",
            "-c1:EFI",
            "-n2:0:0",
            &format!("-t2:{root_type}"),
            "-c2:OsmiumOS",
            &c.disk,
        ]),
    )?;
    succeeds(Command::new("partprobe").arg(&c.disk));
    succeeds(Command::new("udevadm").args(["settle", "--timeout=10"]));
    for n in [1, 2] {
        let p = part_path(&c.disk, n);
        for _ in 0..50 {
            if Path::new(&p).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !Path::new(&p).exists() {
            return Err(format!("Bölüm aygıtı oluşmadı: {p}"));
        }
    }
    Ok(Target { esp: part_path(&c.disk, 1), format_esp: true, xbootldr: None, root: part_path(&c.disk, 2) })
}

fn uuid(dev: &str) -> Result<String> {
    let u = output(Command::new("blkid").args(["-p", "-s", "UUID", "-o", "value", dev]))?;
    let u = u.trim().to_string();
    if u.is_empty() { Err(format!("{dev}: UUID okunamadı")) } else { Ok(u) }
}

fn format_and_mount(c: &Config, t: &Target) -> Result<Layout> {
    let (esp, part) = (t.esp.clone(), t.root.clone());
    let root_dev = if c.encrypt { format!("/dev/mapper/{MAPPER}") } else { part.clone() };

    // A private mount point: otherwise every mount below /mnt is copied into the mount
    // namespaces of running services (resolved, udevd, ...), which then keep the disk busy.
    sh("mount", Command::new("mount").args(["--bind", MNT, MNT]))?;
    sh("mount", Command::new("mount").args(["--make-private", MNT]))?;

    // The FAT format runs alongside LUKS + btrfs.
    std::thread::scope(|s| -> Result {
        let fat = s.spawn(|| -> Result {
            if t.format_esp {
                sh("mkfs.fat", Command::new("mkfs.fat").args(["-F32", "-n", "EFI", &esp]))?;
            }
            match &t.xbootldr {
                Some(x) => sh("mkfs.fat", Command::new("mkfs.fat").args(["-F32", "-n", "OSMIUMBOOT", x])),
                None => Ok(()),
            }
        });
        let main = (|| -> Result {
            if c.encrypt {
                // LUKS2 with argon2id: only the systemd initramfs reads it, never a boot loader.
                let format = ["luksFormat", "--type", "luks2", "--batch-mode", "--iter-time", "2000", "--key-file=-", &part];
                sh_in("cryptsetup luksFormat", Command::new("cryptsetup").args(format), &c.passphrase)?;
                sh_in(
                    "cryptsetup open",
                    Command::new("cryptsetup").args(["open", "--type", "luks", "--key-file=-", &part, MAPPER]),
                    &c.passphrase,
                )?;
            }
            sh("mkfs.btrfs", Command::new("mkfs.btrfs").args(["-f", "-q", "-L", "OsmiumOS", &root_dev]))
        })();
        main.and(join(fat))
    })?;

    // Subvolumes, then the real mounts.
    sh("mount", Command::new("mount").args(["-o", BTRFS_OPTS, &root_dev, MNT]))?;
    for (sub, _) in SUBVOLS {
        sh("btrfs subvolume", Command::new("btrfs").args(["-q", "subvolume", "create", &format!("{MNT}/{sub}")]))?;
    }
    sh("btrfs subvolume set-default", Command::new("btrfs").args(["subvolume", "set-default", &format!("{MNT}/@")]))?;
    sh("umount", Command::new("umount").arg(MNT))?;
    // /.snapshots is mounted after `snapper create-config`, which wants to create it itself.
    for (sub, mp) in SUBVOLS.iter().filter(|(_, mp)| *mp != "/.snapshots") {
        let dir = format!("{MNT}{mp}");
        fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
        sh(
            "mount",
            Command::new("mount").args(["-o", &format!("subvol=/{sub},{BTRFS_OPTS}"), &root_dev, &dir]),
        )?;
    }

    let luks_uuid = if c.encrypt { Some(uuid(&part)?) } else { None };
    if c.encrypt {
        add_keyfile(c, &part)?;
    }
    let xbootldr = match &t.xbootldr {
        Some(x) => Some((x.clone(), uuid(x)?)),
        None => None,
    };
    Ok(Layout { root_uuid: uuid(&root_dev)?, esp_uuid: uuid(&esp)?, esp, luks_uuid, xbootldr })
}

/// Random one-time key for the first-boot TPM2 enrollment. It waits on the tmpfs /run until
/// the system is copied, then moves to TPM_KEY on the encrypted root (see `configure`).
fn add_keyfile(c: &Config, part: &str) -> Result {
    let path = TPM_KEY_TMP;
    let mut key = [0u8; 64];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut key))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    let _ = fs::remove_file(path);
    fs::write(path, "").map_err(|e| format!("{path}: {e}"))?;
    chmod(path, 0o600)?;
    fs::write(path, key).map_err(|e| format!("{path}: {e}"))?;
    // High-entropy key: a tiny iteration time loses nothing and saves a second.
    sh_in(
        "cryptsetup luksAddKey",
        Command::new("cryptsetup").args(["luksAddKey", "--pbkdf", "pbkdf2", "--iter-time", "10", "--key-file=-", part, path]),
        &c.passphrase,
    )
}

#[cfg(unix)]
fn chmod(path: &str, mode: u32) -> Result {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| format!("{path}: {e}"))
}

#[cfg(not(unix))]
fn chmod(_: &str, _: u32) -> Result {
    Ok(())
}

fn copy_system() -> Result {
    let sfs = format!("{BOOTMNT}/arch/x86_64/airootfs.sfs");
    // Not `unsquashfs -version`: it exits non-zero, which silently selected the slow rsync path.
    if Path::new(&sfs).is_file() && Path::new("/usr/bin/unsquashfs").is_file() {
        unsquash(&sfs)?;
    } else {
        // Without squashfs-tools: copy the already-mounted live root.
        sh(
            "rsync",
            Command::new("rsync").args(["-aHAX", "--numeric-ids", &format!("{LIVE_ROOT}/"), &format!("{MNT}/")]),
        )?;
    }
    // The ISO keeps the kernel (vmlinuz-linux-zen) outside the squashfs. postinstall also does
    // this; harmless duplicate.
    let boot = format!("{BOOTMNT}/arch/boot/x86_64");
    for e in fs::read_dir(&boot).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("vmlinuz-") {
            let _ = fs::create_dir_all(format!("{MNT}/boot"));
            fs::copy(e.path(), format!("{MNT}/boot/{name}")).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    // Microcode images, which mkarchiso also keeps outside the squashfs (owned by *-ucode packages).
    for img in ["amd-ucode.img", "intel-ucode.img"] {
        let src = format!("{BOOTMNT}/arch/boot/{img}");
        if Path::new(&src).is_file() {
            fs::copy(&src, format!("{MNT}/boot/{img}")).map_err(|e| format!("{img}: {e}"))?;
        }
    }
    Ok(())
}

fn unsquash(sfs: &str) -> Result {
    let procs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).to_string();
    let mut child = Command::new("unsquashfs")
        .args(["-f", "-percentage", "-processors", &procs, "-d", MNT, sfs])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("unsquashfs: {e}"))?;
    let mut err = child.stderr.take().ok_or("unsquashfs: stderr")?;
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let out = child.stdout.take().ok_or("unsquashfs: stdout")?;
    // Progress lines end in \n or \r.
    let (mut line, mut last) = (Vec::new(), 0u8);
    for b in io::BufReader::new(out).bytes().map_while(|b| b.ok()) {
        if b == b'\n' || b == b'\r' {
            if let Some(p) = parse_pct(&String::from_utf8_lossy(&line)).filter(|&p| p > last) {
                last = p;
                emit("copy", copy_pct(p), plan_step("copy").1);
            }
            line.clear();
        } else {
            line.push(b);
        }
    }
    let status = child.wait().map_err(|e| format!("unsquashfs: {e}"))?;
    let err = err_thread.join().unwrap_or_default();
    if status.success() { Ok(()) } else { Err(format!("unsquashfs başarısız ({status}): {}", tail(&err, 400))) }
}

fn write_to(rel: &str, text: &str) -> Result {
    let p = format!("{MNT}{rel}");
    if let Some(dir) = Path::new(&p).parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&p, text).map_err(|e| format!("{p}: {e}"))
}

/// Pseudo filesystems for the chroot; torn down by `cleanup` (umount -R).
fn chroot_mounts() -> Result {
    let m = |what: &str, args: &[&str]| sh(what, Command::new("mount").args(args));
    m("mount proc", &["-t", "proc", "proc", &format!("{MNT}/proc")])?;
    m("mount sys", &["-t", "sysfs", "sys", &format!("{MNT}/sys")])?;
    m("mount dev", &["--rbind", "/dev", &format!("{MNT}/dev")])?;
    m("mount dev", &["--make-rslave", &format!("{MNT}/dev")])?;
    m("mount run", &["-t", "tmpfs", "-o", "mode=0755,nosuid,nodev", "tmpfs", &format!("{MNT}/run")])?;
    // The image's /etc/resolv.conf points into /run: without the live resolver's files the
    // chroot has no DNS and dots from GitHub cannot be fetched.
    let resolve = "/run/systemd/resolve";
    if Path::new(resolve).is_dir() && fs::create_dir_all(format!("{MNT}{resolve}")).is_ok() {
        for f in ["stub-resolv.conf", "resolv.conf"] {
            let _ = fs::copy(format!("{resolve}/{f}"), format!("{MNT}{resolve}/{f}"));
        }
    }
    if Path::new("/run/udev").is_dir() {
        let _ = fs::create_dir_all(format!("{MNT}/run/udev"));
        m("mount udev", &["--bind", "/run/udev", &format!("{MNT}/run/udev")])?;
    }
    if Path::new("/sys/firmware/efi/efivars").is_dir() {
        // Optional: without it bootctl only installs the fallback path, and keys are not enrolled.
        succeeds(Command::new("mount").args(["-t", "efivarfs", "efivarfs", &format!("{MNT}/sys/firmware/efi/efivars")]));
    }
    Ok(())
}

fn configure(c: &Config, lay: &Layout) -> Result {
    chroot_mounts()?;
    emit("config", plan_step("config").0, "Yapılandırma dosyaları yazılıyor");

    // Host-side file writes: milliseconds.
    write_to("/etc/fstab", &fstab(&lay.root_uuid, &lay.esp_uuid, lay.xbootldr.as_ref().map(|(_, u)| u.as_str())))?;
    if lay.xbootldr.is_some() {
        // osmium-boot then puts the UKIs on it instead of the (Windows) ESP.
        write_to("/etc/osmium/boot.conf", "xbootldr=/boot\n")?;
    }
    write_to("/etc/kernel/cmdline", &kernel_cmdline(&lay.root_uuid, lay.luks_uuid.as_deref()))?;
    if lay.luks_uuid.is_some() {
        let dst = format!("{MNT}{TPM_KEY}");
        fs::create_dir_all(format!("{MNT}/var/lib/osmium/boot")).map_err(|e| format!("{dst}: {e}"))?;
        fs::copy(TPM_KEY_TMP, &dst).map_err(|e| format!("{dst}: {e}"))?;
        chmod(&dst, 0o600)?;
        let _ = fs::remove_file(TPM_KEY_TMP);
    }
    write_to("/etc/machine-id", "")?;
    write_to("/etc/hostname", &format!("{}\n", c.hostname))?;
    write_to("/etc/hosts", &hosts(&c.hostname))?;
    write_to("/etc/locale.gen", &locale_gen(&c.locale))?;
    write_to("/etc/locale.conf", &format!("LANG={}\n", c.locale))?;
    write_to("/etc/vconsole.conf", &format!("KEYMAP={}\n", c.keymap))?;
    write_to("/etc/X11/xorg.conf.d/00-keyboard.conf", &xorg_keyboard(&c.xkblayout))?;
    write_to("/etc/sudoers.d/10-wheel", "%wheel ALL=(ALL:ALL) ALL\n")?;
    chmod(&format!("{MNT}/etc/sudoers.d/10-wheel"), 0o440)?;
    write_to("/etc/mkinitcpio.conf.d/10-osmium.conf", &mkinitcpio_dropin(c.encrypt))?;
    sh(
        "ln",
        Command::new("ln").args(["-sf", &format!("/usr/share/zoneinfo/{}", c.timezone), &format!("{MNT}/etc/localtime")]),
    )?;

    // locale-gen (+ clock) is independent of user creation + postinstall.
    std::thread::scope(|s| -> Result {
        let loc = s.spawn(|| -> Result {
            sh("locale-gen", &mut ch("locale-gen", &[]))?;
            succeeds(&mut ch("hwclock", &["--systohc"]));
            Ok(())
        });
        let usr = s.spawn(|| -> Result {
            let groups = filter_groups(&fs::read_to_string(format!("{MNT}/etc/group")).unwrap_or_default()).join(",");
            sh(
                "useradd",
                &mut ch("useradd", &["-m", "-G", &groups, "-s", "/bin/bash", "-c", &c.fullname, &c.username]),
            )?;
            sh_in("chpasswd", &mut ch("chpasswd", &[]), &format!("{}:{}\n", c.username, c.password))?;
            // After useradd: both take the passwd lock. Removes the live user, writes the preset.
            sh("osmium-postinstall", &mut ch("/usr/bin/osmium-postinstall", &[]))
        });
        join(loc).and(join(usr))
    })
}

/// UKI, systemd-boot, Secure Boot keys, services and the desktop dots, as parallel as their
/// dependencies allow.
fn boot(c: &Config, lay: &Layout) -> std::result::Result<(), StepErr> {
    let esp_dir = format!("{MNT}{ESP_MOUNT}");
    fs::create_dir_all(&esp_dir).map_err(|e| ("boot", format!("{esp_dir}: {e}")))?;
    sh("mount ESP", Command::new("mount").args(["-o", "umask=0077", &lay.esp, &esp_dir])).map_err(at("boot"))?;
    snapshots_subvol(lay).map_err(at("boot"))?;

    std::thread::scope(|s| {
        let loader = s.spawn(|| -> Result { install_loader(&c.disk, &lay.esp, lay.xbootldr.is_some()) });
        let services = s.spawn(|| -> Result {
            for (unit, on) in SERVICES {
                // Missing units are fine (edition-dependent).
                succeeds(Command::new("systemctl").arg(format!("--root={MNT}")).arg(if *on { "enable" } else { "disable" }).arg(unit));
            }
            Ok(())
        });
        let dots = s.spawn(|| -> Result {
            let (home, user) = (format!("HOME=/home/{}", c.username), format!("USER={}", c.username));
            let logname = format!("LOGNAME={}", c.username);
            let r = sh(
                "osmium-dots",
                &mut ch(
                    "runuser",
                    &["-u", &c.username, "--", "env", &home, &user, &logname, "osmium-dots", "apply", &c.dots, "--mode", &c.dots_mode],
                ),
            );
            // Not fatal: the system boots without the desktop dots.
            if let Err(e) = r {
                emit("dots", plan_step("boot").0, &format!("uyarı: masaüstü yapılandırması uygulanamadı: {e}"));
            }
            // The dots ship a Turkish keyboard; the compositor takes its layout from them.
            if c.xkblayout.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == ',' || ch == '_') {
                apply_kb_layout(Path::new(&format!("{MNT}/home/{}/.config/hypr", c.username)), &c.xkblayout);
            }
            Ok(())
        });

        let initramfs = sh("mkinitcpio", &mut ch("mkinitcpio", &["-P"]));
        let loader = join(loader);
        let _ = join(services);
        let _ = join(dots);
        initramfs.map_err(at("boot"))?;
        loader.map_err(at("boot"))?;
        // Signs the UKI (keys were made by install_loader) and puts it on the ESP.
        sh("osmium-boot", &mut ch("osmium-boot", &["kur", "--ilk"])).map_err(at("boot"))?;
        // First rollback point. Not fatal.
        succeeds(&mut ch("snapper", &["--no-dbus", "-c", "root", "create", "-d", "Kurulum"]));
        Ok(())
    })
}

/// `snapper create-config` (run by osmium-postinstall) made /.snapshots a subvolume inside @,
/// which a rollback would leave behind. Replace it with the top-level @snapshots.
fn snapshots_subvol(lay: &Layout) -> Result {
    let dir = format!("{MNT}/.snapshots");
    if succeeds(Command::new("btrfs").args(["subvolume", "show", &dir])) && !succeeds(Command::new("mountpoint").args(["-q", &dir])) {
        sh("btrfs subvolume delete", Command::new("btrfs").args(["-q", "subvolume", "delete", &dir]))?;
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    let dev = match &lay.luks_uuid {
        Some(_) => format!("/dev/mapper/{MAPPER}"),
        None => format!("/dev/disk/by-uuid/{}", lay.root_uuid),
    };
    sh("mount", Command::new("mount").args(["-o", &format!("subvol=/@snapshots,{BTRFS_OPTS}"), &dev, &dir]))?;
    chmod(&dir, 0o750)
}

/// Secure Boot keys (enrolled right away when the firmware is in Setup Mode), a signed
/// systemd-boot, and the loader config. The UKI itself comes later (`osmium-boot kur`).
fn install_loader(disk: &str, esp: &str, xbootldr: bool) -> Result {
    remove_old_boot_entries();
    sh("sbctl create-keys", &mut ch("sbctl", &["create-keys"]))?;
    let stub = "/usr/lib/systemd/boot/efi/systemd-bootx64.efi";
    sh("sbctl sign", &mut ch("sbctl", &["sign", "-s", "-o", &format!("{stub}.signed"), stub]))?;
    // bootctl checks the partition type through udev; the chroot sees the live system's.
    let bootctl = |extra: &[&str]| {
        let boot: &[&str] = if xbootldr { &["--boot-path", "/boot"] } else { &[] };
        let mut c = ch("bootctl", &[&["install", "--esp-path", ESP_MOUNT], boot, extra].concat());
        c.env("SYSTEMD_RELAX_ESP_CHECKS", "1");
        sh("bootctl install", &mut c)
    };
    // In the chroot bootctl cannot see the ESP's partition GUID and would write a boot entry
    // pointing nowhere (only the fallback /EFI/BOOT/BOOTX64.EFI path then boots, and next to
    // Windows the firmware starts Windows). The entry is made from outside instead.
    bootctl(&["--no-variables"])?;
    if let Err(e) = add_boot_entry(disk, esp) {
        emit("boot", plan_step("boot").0, &format!("uyarı: UEFI açılış girdisi eklenemedi, yedek yoldan açılacak: {e}"));
    }
    // Next to Windows the menu shows for a moment, so Windows can be picked.
    let conf = if xbootldr { LOADER_CONF.replace("timeout 0", "timeout 3") } else { LOADER_CONF.to_string() };
    write_to(&format!("{ESP_MOUNT}/loader/loader.conf"), &conf)?;
    if setup_mode() {
        // Microsoft's keys stay: GPU and network option ROMs are signed with them.
        if let Err(e) = sh("sbctl enroll-keys", &mut ch("sbctl", &["enroll-keys", "--microsoft"])) {
            emit("boot", plan_step("boot").0, &format!("uyarı: Güvenli Önyükleme anahtarları yüklenemedi: {e}"));
        }
    }
    Ok(())
}

/// NVRAM entry for systemd-boot on `esp`, first in BootOrder (efibootmgr puts new entries first).
fn add_boot_entry(disk: &str, esp: &str) -> Result {
    if !Path::new("/sys/firmware/efi/efivars").is_dir() {
        return Ok(());
    }
    let n = part_number(disk, esp).ok_or_else(|| format!("{esp}: bölüm numarası okunamadı"))?;
    sh(
        "efibootmgr",
        Command::new("efibootmgr").args([
            "--quiet", "--create", "--disk", disk, "--part", &n.to_string(), "--label", "OsmiumOS",
            "--loader", "\\EFI\\systemd\\systemd-bootx64.efi",
        ]),
    )
}

/// The firmware accepts new Secure Boot keys without a signature (SetupMode = 1).
fn setup_mode() -> bool {
    fs::read("/sys/firmware/efi/efivars/SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c").is_ok_and(|b| b.get(4) == Some(&1))
}

/// Sets `kb_layout` in every Lua/hyprlang file under `dir`. Runs as root on a user-owned tree,
/// so symlinks are never followed; rewriting in place keeps the owner.
fn apply_kb_layout(dir: &Path, layout: &str) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(meta) = p.symlink_metadata() else { continue };
        if meta.is_dir() {
            apply_kb_layout(&p, layout);
        } else if meta.is_file() {
            let lua = match p.extension().and_then(|x| x.to_str()) {
                Some("lua") => true,
                Some("conf") => false,
                _ => continue,
            };
            if let Some(new) = fs::read_to_string(&p).ok().and_then(|t| set_kb_layout(&t, layout, lua)) {
                let _ = fs::write(&p, new);
            }
        }
    }
}

/// `Boot000N` numbers of NVRAM entries labelled `label`, from `efibootmgr` output.
fn boot_entries(efibootmgr: &str, label: &str) -> Vec<String> {
    efibootmgr
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("Boot")?;
            let (num, rest) = rest.split_at_checked(4)?;
            // `*` marks an active entry, a space an inactive one.
            let name = rest.strip_prefix(['*', ' '])?.strip_prefix(' ')?;
            // The label ends at a tab (efibootmgr >= 18) or at two spaces before the path.
            let name = name.split(['\t']).next()?.split("  ").next()?.trim_end();
            (num.chars().all(|c| c.is_ascii_hexdigit()) && name == label).then(|| num.to_string())
        })
        .collect()
}

/// A reinstall would otherwise leave the old entry pointing at the old ESP next to the new one.
fn remove_old_boot_entries() {
    let Ok(list) = output(&mut Command::new("efibootmgr")) else { return };
    for num in boot_entries(&list, "OsmiumOS") {
        succeeds(Command::new("efibootmgr").args(["-q", "-b", &num, "-B"]));
    }
}

// ---------------------------------------------------------------- next to Windows

/// Smallest OsmiumOS install next to Windows, in GiB.
const MIN_GB: u64 = 20;
/// Our XBOOTLDR partition: kernels and UKIs, since Windows' ESP is usually 100-260 MB.
const XBOOTLDR_BYTES: u64 = 1 << 30;
/// Room Windows keeps above what its NTFS needs today.
const WINDOWS_SLACK: u64 = 10 << 30;
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

#[derive(Debug, Clone, PartialEq)]
struct Windows {
    esp: String,
    ntfs: String,
    ntfs_size: u64,
    /// Smallest size ntfsresize accepts for the NTFS partition.
    ntfs_min: u64,
    /// Largest unpartitioned region of the disk.
    free: u64,
    /// Why Windows cannot be shrunk: "bitlocker", "hibernated", "ntfs" or "".
    blocker: String,
}

impl Windows {
    /// Bytes OsmiumOS can get: free space as it is, or what shrinking NTFS gives.
    fn available(&self) -> u64 {
        let shrink = if self.blocker.is_empty() { self.ntfs_size.saturating_sub(self.ntfs_min + WINDOWS_SLACK) } else { 0 };
        self.free.max(shrink).saturating_sub(XBOOTLDR_BYTES + 16 * MIB)
    }

    fn json(&self) -> String {
        format!(
            "{{\"esp\":{},\"ntfs\":{},\"available\":{},\"blocker\":{}}}",
            json_str(&self.esp),
            json_str(&self.ntfs),
            self.available(),
            json_str(&self.blocker)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Part {
    path: String,
    size: u64,
    fstype: String,
    parttype: String,
}

/// Partitions from `lsblk -n -b -P -o PATH,SIZE,FSTYPE,PARTTYPE,TYPE <disk>`.
fn parse_parts(lsblk: &str) -> Vec<Part> {
    lsblk
        .lines()
        .map(parse_pairs)
        .filter(|m| m.get("TYPE").is_some_and(|t| t == "part"))
        .filter_map(|m| {
            Some(Part {
                path: m.get("PATH")?.clone(),
                size: m.get("SIZE")?.parse().ok()?,
                fstype: m.get("FSTYPE").cloned().unwrap_or_default(),
                parttype: m.get("PARTTYPE").cloned().unwrap_or_default().to_lowercase(),
            })
        })
        .collect()
}

/// `ntfsresize --info`: "You might resize at 123456789 bytes or ...".
fn ntfs_min_size(info: &str) -> Option<u64> {
    let rest = &info[info.find("resize at ")? + "resize at ".len()..];
    rest.split_whitespace().next()?.parse().ok()
}

/// Largest free region from `sfdisk --list-free --bytes` (`Start End Sectors Size` rows).
fn largest_free(text: &str) -> u64 {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() == 4 && f[0].parse::<u64>().is_ok()).then(|| f[3].parse::<u64>().ok()).flatten()
        })
        .max()
        .unwrap_or(0)
}

/// Why ntfsresize refuses, from its output; "" when it would resize.
fn ntfs_blocker(info: &str, ok: bool) -> &'static str {
    let lower = info.to_lowercase();
    if lower.contains("hibernat") || lower.contains("fast restart") {
        "hibernated"
    } else if ok && ntfs_min_size(info).is_some() {
        ""
    } else {
        "ntfs"
    }
}

/// Number of a partition device of `disk` (`/dev/nvme0n1p3` -> 3).
fn part_number(disk: &str, part: &str) -> Option<u32> {
    let rest = part.strip_prefix(disk)?;
    rest.strip_prefix('p').unwrap_or(rest).parse().ok()
}

/// The new NTFS partition size in MiB when it has to shrink for `need` bytes; None when the
/// free space is enough as it is.
fn alongside_plan(win: Option<&Windows>, size_gb: u64) -> Result<Option<u64>> {
    let w = win.ok_or("Bu diskte Windows bulunamadı.")?;
    let need = size_gb * GIB + XBOOTLDR_BYTES + 16 * MIB;
    if w.free >= need {
        return Ok(None);
    }
    match w.blocker.as_str() {
        "bitlocker" => return Err("Windows bölümü BitLocker ile şifreli. Windows'ta BitLocker'ı kapatın (ya da askıya alın), sonra yeniden deneyin.".into()),
        "hibernated" => return Err("Windows hazırda bekletmede (Hızlı Başlatma). Windows'ta Hızlı Başlatma'yı kapatıp \"Yeniden başlat\" ile kapatın, sonra yeniden deneyin.".into()),
        "" => {}
        _ => return Err("Windows bölümü küçültülemiyor (NTFS hatası). Windows'ta \"chkdsk /f\" çalıştırıp yeniden deneyin.".into()),
    }
    if size_gb * GIB > w.available() {
        return Err(format!("Windows'tan en fazla {} GB ayrılabilir.", w.available() / GIB));
    }
    Ok(Some((w.ntfs_size - need) / MIB))
}

/// What is on `disk` for an install next to Windows: an ESP with Windows Boot Manager and
/// its largest NTFS (or BitLocker) partition. None without Windows.
fn probe_windows(disk: &str) -> Option<Windows> {
    let lsblk = output(Command::new("lsblk").args(["-n", "-b", "-P", "-o", "PATH,SIZE,FSTYPE,PARTTYPE,TYPE", disk])).ok()?;
    let parts = parse_parts(&lsblk);
    let esp = parts.iter().find(|p| p.parttype == ESP_TYPE && windows_esp(&p.path))?;
    let data = parts.iter().filter(|p| matches!(p.fstype.as_str(), "ntfs" | "BitLocker")).max_by_key(|p| p.size)?;
    let free = largest_free(&output(Command::new("sfdisk").args(["--list-free", "--bytes", disk])).unwrap_or_default());
    let (ntfs_min, blocker) = if data.fstype == "BitLocker" {
        (data.size, "bitlocker")
    } else {
        let out = Command::new("ntfsresize").args(["--info", "--force", "--no-progress-bar", &data.path]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (ntfs_min_size(&text).unwrap_or(data.size), ntfs_blocker(&text, out.status.success()))
    };
    Some(Windows { esp: esp.path.clone(), ntfs: data.path.clone(), ntfs_size: data.size, ntfs_min, free, blocker: blocker.into() })
}

/// Windows Boot Manager on an ESP, looked at read-only.
fn windows_esp(dev: &str) -> bool {
    let dir = "/run/osmium-install-esp";
    if fs::create_dir_all(dir).is_err() || !succeeds(Command::new("mount").args(["-t", "vfat", "-o", "ro", dev, dir])) {
        return false;
    }
    let found = ["EFI/Microsoft/Boot/bootmgfw.efi", "EFI/MICROSOFT/BOOT/BOOTMGFW.EFI"].iter().any(|f| Path::new(dir).join(f).is_file());
    succeeds(Command::new("umount").arg(dir));
    found
}

fn partition_numbers(disk: &str) -> Vec<u32> {
    let parts = output(Command::new("lsblk").args(["-n", "-r", "-o", "PATH,TYPE", disk])).unwrap_or_default();
    parts.lines().filter_map(|l| l.split_once(' ')).filter(|(_, t)| *t == "part").filter_map(|(p, _)| part_number(disk, p)).collect()
}

/// Adds one partition in the largest free block and returns its device.
fn new_partition(disk: &str, size: &str, code: &str, name: &str) -> Result<String> {
    let before = partition_numbers(disk);
    sh("sgdisk", Command::new("sgdisk").args([&format!("-n0:0:{size}"), &format!("-t0:{code}"), &format!("-c0:{name}"), disk]))?;
    succeeds(Command::new("partprobe").arg(disk));
    succeeds(Command::new("udevadm").args(["settle", "--timeout=10"]));
    let n = partition_numbers(disk).into_iter().find(|n| !before.contains(n)).ok_or("Yeni bölüm görünmedi.")?;
    let p = part_path(disk, n as u8);
    for _ in 0..50 {
        if Path::new(&p).exists() {
            return Ok(p);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("Bölüm aygıtı oluşmadı: {p}"))
}

/// Shrinks Windows if needed, then adds our boot and root partitions. Never deletes anything.
fn prepare_alongside(c: &Config) -> Result<Target> {
    let win = probe_windows(&c.disk);
    let shrink = alongside_plan(win.as_ref(), c.size_gb)?;
    let w = win.ok_or("Windows bulunamadı.")?;
    if let Some(mib) = shrink {
        emit("disk", plan_step("disk").0, &format!("Windows bölümü {} GB'a küçültülüyor", mib * MIB / GIB));
        let n = part_number(&c.disk, &w.ntfs).ok_or("NTFS bölüm numarası okunamadı.")?;
        // The file system first, 1 MiB under the new partition end; then the partition.
        let fs_bytes = (mib - 1) * MIB;
        sh_in("ntfsresize", Command::new("ntfsresize").args(["--force", "--no-progress-bar", "--size", &fs_bytes.to_string(), &w.ntfs]), "y\n")?;
        sh_in(
            "sfdisk",
            Command::new("sfdisk").args(["--no-reread", "--force", "-N", &n.to_string(), &c.disk]),
            &format!(", {mib}MiB\n"),
        )?;
        succeeds(Command::new("partprobe").arg(&c.disk));
        succeeds(Command::new("udevadm").args(["settle", "--timeout=10"]));
    }
    let xbootldr = new_partition(&c.disk, "+1GiB", "ea00", "OsmiumBoot")?;
    let code = if c.encrypt { "8309" } else { "8300" };
    let root = new_partition(&c.disk, &format!("+{}GiB", c.size_gb), code, "OsmiumOS")?;
    for p in [&xbootldr, &root] {
        succeeds(Command::new("wipefs").args(["-a", "-f", p]));
    }
    Ok(Target { esp: w.esp, format_esp: false, xbootldr: Some(xbootldr), root })
}

/// Next to Windows, /boot is our XBOOTLDR partition; the copied /boot (on btrfs) moves onto it.
fn mount_xbootldr(lay: &Layout) -> Result {
    let Some((dev, _)) = &lay.xbootldr else { return Ok(()) };
    let (boot, old) = (format!("{MNT}/boot"), format!("{MNT}/boot.kopya"));
    fs::rename(&boot, &old).map_err(|e| format!("{boot}: {e}"))?;
    fs::create_dir_all(&boot).map_err(|e| format!("{boot}: {e}"))?;
    sh("mount", Command::new("mount").args(["-o", "umask=0077", dev, &boot]))?;
    // FAT keeps no owners or modes: copy the contents only.
    sh("cp", Command::new("cp").args(["-rT", "--no-preserve=mode,ownership", &old, &boot]))?;
    fs::remove_dir_all(&old).map_err(|e| format!("{old}: {e}"))
}

/// Unmounts everything under /mnt and closes LUKS. Safe to call at any point.
fn cleanup() {
    succeeds(&mut Command::new("sync"));
    // Daemons started inside the chroot (gpg-agent from pacman-key) keep /mnt and LUKS busy.
    let pids: Vec<String> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| fs::read_link(e.path().join("root")).is_ok_and(|r| r == Path::new(MNT)))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    if !pids.is_empty() {
        succeeds(Command::new("kill").arg("-9").args(&pids));
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // Mounts are stacked on /mnt (private bind below, root on top): unmount until none is left.
    for _ in 0..4 {
        if !succeeds(Command::new("mountpoint").args(["-q", MNT])) {
            break;
        }
        if !succeeds(Command::new("umount").args(["-R", MNT])) {
            succeeds(Command::new("umount").args(["-R", "-l", MNT]));
        }
    }
    // udev may still be probing the mapping right after the unmount.
    for _ in 0..10 {
        succeeds(Command::new("udevadm").args(["settle", "--timeout=5"]));
        if !Path::new(&format!("/dev/mapper/{MAPPER}")).exists() || succeeds(Command::new("cryptsetup").args(["close", MAPPER])) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_old_boot_entries() {
        let list = "BootCurrent: 0001\nTimeout: 0 seconds\nBootOrder: 0003,0001,0000\n\
            Boot0000* UiApp\tFvVol(7cb8bdc9)/FvFile(462caa21)\n\
            Boot0001* UEFI QEMU HARDDISK  PciRoot(0x0)/Pci(0x1,0x1)\n\
            Boot0003* OsmiumOS\tHD(1,GPT,abc)/File(\\EFI\\OsmiumOS\\grubx64.efi)\n\
            Boot000A  OsmiumOS  HD(1,GPT,def)/File(\\EFI\\OsmiumOS\\grubx64.efi)\n\
            Boot0004* OsmiumOS Yedek\tHD(1,GPT,abc)\n\
            Boot0005* osmiumos\tHD(1,GPT,abc)\n";
        assert_eq!(boot_entries(list, "OsmiumOS"), ["0003", "000A"]);
        assert!(boot_entries("", "OsmiumOS").is_empty());
    }

    const LSBLK: &str = r#"PATH="/dev/sda" SIZE="500107862016" MODEL="Samsung SSD 860 " RM="0" TRAN="sata" TYPE="disk" RO="0"
PATH="/dev/sdb" SIZE="16008609792" MODEL="USB\x20DISK 3.0" RM="1" TRAN="usb" TYPE="disk" RO="0"
PATH="/dev/nvme0n1" SIZE="1000204886016" MODEL="WD \x22Black\x22" RM="0" TRAN="nvme" TYPE="disk" RO="0"
PATH="/dev/zram0" SIZE="4294967296" MODEL="" RM="0" TRAN="" TYPE="disk" RO="0"
PATH="/dev/loop0" SIZE="800000000" MODEL="" RM="0" TRAN="" TYPE="loop" RO="1"
PATH="/dev/sr0" SIZE="1073741312" MODEL="DVD" RM="1" TRAN="sata" TYPE="rom" RO="0"
PATH="/dev/sdc" SIZE="0" MODEL="Reader" RM="1" TRAN="usb" TYPE="disk" RO="0"
PATH="/dev/sdd" SIZE="1000" MODEL="Locked" RM="0" TRAN="sata" TYPE="disk" RO="1"
"#;

    fn good() -> Config {
        parse_config(
            "disk=/dev/sda\nencrypt=0\nhostname=osmium-pc\nusername=ali\nfullname=Ali Veli\npassword=p=a ss\ntimezone=Europe/Istanbul\nlocale=tr_TR.UTF-8\nkeymap=trq\nxkblayout=tr\ndots=ii\ndots_mode=config\n",
        )
    }

    #[test]
    fn pairs_unescape() {
        let m = parse_pairs(r#"PATH="/dev/sdb" MODEL="USB\x20DISK \x22x\x22" RM="1""#);
        assert_eq!(m["MODEL"], "USB DISK \"x\"");
        assert_eq!(m["RM"], "1");
        assert_eq!(unescape_lsblk(r"caf\xc3\xa9"), "café");
        assert_eq!(unescape_lsblk(r"bad\xZZ"), r"bad\xZZ");
    }

    #[test]
    fn disks_filtered() {
        let d = filter_disks(LSBLK, &["/dev/sdb".to_string()]);
        let paths: Vec<_> = d.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/dev/sda", "/dev/nvme0n1"]);
        assert_eq!(d[0].model, "Samsung SSD 860");
        assert_eq!(d[0].transport, "sata");
        assert!(!d[0].removable);
        let all = filter_disks(LSBLK, &[]);
        assert!(all.iter().any(|d| d.path == "/dev/sdb" && d.removable && d.model == "USB DISK 3.0"));
    }

    #[test]
    fn disk_json() {
        let d = filter_disks(LSBLK, &[]);
        let j = d.iter().find(|d| d.path == "/dev/nvme0n1").unwrap().json();
        assert_eq!(
            j,
            r#"{"path":"/dev/nvme0n1","model":"WD \"Black\"","size":1000204886016,"removable":false,"transport":"nvme","windows":null}"#
        );
    }

    #[test]
    fn config_parse_keeps_secrets_verbatim() {
        let c = good();
        assert_eq!(c.password, "p=a ss");
        assert_eq!(c.fullname, "Ali Veli");
        assert!(!c.encrypt);
        let c = parse_config("encrypt=1\r\npassphrase= x=y \r\nbogus\r\n");
        assert!(c.encrypt);
        assert_eq!(c.passphrase, " x=y ");
        assert_eq!(c.dots, "ii");
    }

    #[test]
    fn validation() {
        let ok = |_: &str| true;
        assert!(validate(&good(), &ok).is_ok());
        let bad = |f: &dyn Fn(&mut Config)| {
            let mut c = good();
            f(&mut c);
            validate(&c, &ok).is_err()
        };
        assert!(bad(&|c| c.username = "Ali".into()));
        assert!(bad(&|c| c.username = "1ali".into()));
        assert!(bad(&|c| c.username = "root".into()));
        assert!(bad(&|c| c.username = "a".repeat(33)));
        assert!(bad(&|c| c.hostname = "-x".into()));
        assert!(bad(&|c| c.hostname = "a b".into()));
        assert!(bad(&|c| c.password = String::new()));
        assert!(bad(&|c| c.encrypt = true));
        assert!(bad(&|c| c.timezone = "../etc".into()));
        assert!(bad(&|c| c.locale = "tr_TR".into()));
        assert!(bad(&|c| c.dots = "github:a".into()));
        assert!(bad(&|c| c.dots = "github:../x".into()));
        assert!(bad(&|c| c.dots_mode = "x".into()));
        assert!(bad(&|c| c.fullname = "a:b".into()));
        assert!(bad(&|c| c.disk = "sda".into()));
        assert!(validate(&good(), &|_| false).is_err());
        let mut c = good();
        c.dots = "github:end-4/dots-hyprland".into();
        c.encrypt = true;
        c.passphrase = "x".into();
        assert!(validate(&c, &ok).is_ok());
    }

    #[test]
    fn partition_names() {
        assert_eq!(part_path("/dev/sda", 1), "/dev/sda1");
        assert_eq!(part_path("/dev/nvme0n1", 2), "/dev/nvme0n1p2");
        assert_eq!(part_path("/dev/mmcblk0", 1), "/dev/mmcblk0p1");
    }

    #[test]
    fn generated_text() {
        let x = fstab("RRR", "EEE", Some("XXX"));
        assert!(x.ends_with("UUID=XXX /boot vfat umask=0077 0 2\n"));
        let f = fstab("RRR", "EEE", None);
        assert!(!f.contains("/boot "));
        assert!(f.contains("UUID=RRR / btrfs noatime,compress=zstd:1 0 0\n"));
        assert!(!f.contains("subvol=/@,"));
        assert!(f.contains("UUID=RRR /.snapshots btrfs subvol=/@snapshots,"));
        assert!(f.contains("UUID=RRR /home btrfs subvol=/@home,"));
        assert!(f.contains("UUID=RRR /var/cache btrfs subvol=/@cache,"));
        assert!(f.contains("UUID=RRR /var/log btrfs subvol=/@log,"));
        assert!(f.contains("UUID=EEE /efi vfat umask=0077 0 2\n"));
        assert_eq!(hosts("pc"), "127.0.0.1 localhost\n::1 localhost\n127.0.1.1 pc.localdomain pc\n");
        assert_eq!(locale_gen("tr_TR.UTF-8"), "en_US.UTF-8 UTF-8\ntr_TR.UTF-8 UTF-8\n");
        assert_eq!(locale_gen("en_US.UTF-8"), "en_US.UTF-8 UTF-8\n");
        assert!(xorg_keyboard("tr").contains("\"XkbLayout\" \"tr\""));
    }

    const WIN_LSBLK: &str = r#"PATH="/dev/nvme0n1p1" SIZE="104857600" FSTYPE="vfat" PARTTYPE="c12a7328-f81f-11d2-ba4b-00a0c93ec93b" TYPE="part"
PATH="/dev/nvme0n1p2" SIZE="16777216" FSTYPE="" PARTTYPE="e3c9e316-0b5c-4db8-817d-f92df00215ae" TYPE="part"
PATH="/dev/nvme0n1p3" SIZE="499000000000" FSTYPE="ntfs" PARTTYPE="ebd0a0a2-b9e5-4433-87c0-68b6b72699c7" TYPE="part"
PATH="/dev/nvme0n1p4" SIZE="700000000" FSTYPE="ntfs" PARTTYPE="de94bba4-06d1-4d40-a16a-bfd50179d6ac" TYPE="part""#;

    #[test]
    fn reads_a_windows_disk() {
        let parts = parse_parts(WIN_LSBLK);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].parttype, ESP_TYPE);
        assert_eq!(parts[2].fstype, "ntfs");
        assert_eq!(part_number("/dev/nvme0n1", "/dev/nvme0n1p3"), Some(3));
        assert_eq!(part_number("/dev/sda", "/dev/sda12"), Some(12));
        assert_eq!(part_number("/dev/sda", "/dev/sdb1"), None);
        let info = "ntfsresize v2022.10.3\nDevice name        : /dev/sda3\nYou might resize at 53687091200 bytes or 53688 MB (freeing 445312 MB).\n";
        assert_eq!(ntfs_min_size(info), Some(53687091200));
        assert_eq!(ntfs_blocker(info, true), "");
        assert_eq!(ntfs_blocker("ERROR: The NTFS partition is hibernated. Windows must be resumed", false), "hibernated");
        assert_eq!(ntfs_blocker("ERROR: volume is dirty", false), "ntfs");
        let free = "Disk /dev/sda: 100 GiB\n\n   Start       End  Sectors        Size\n    2048      4095     2048     1048576\n 9000000 209715166 200715167 102766165504\n";
        assert_eq!(largest_free(free), 102766165504);
        assert_eq!(largest_free(""), 0);
    }

    #[test]
    fn plans_the_shrink() {
        let w = Windows {
            esp: "/dev/sda1".into(),
            ntfs: "/dev/sda3".into(),
            ntfs_size: 400 * GIB,
            ntfs_min: 100 * GIB,
            free: 0,
            blocker: String::new(),
        };
        // 400 - 100 - 10 slack = 290 GiB, minus the boot partition.
        assert!(w.available() > 288 * GIB && w.available() < 290 * GIB);
        let mib = alongside_plan(Some(&w), 50).unwrap().unwrap();
        assert_eq!(mib * MIB, 400 * GIB - 50 * GIB - XBOOTLDR_BYTES - 16 * MIB);
        assert!(alongside_plan(Some(&w), 300).unwrap_err().contains("en fazla 288 GB"));
        let free = Windows { free: 80 * GIB, ..w.clone() };
        assert_eq!(alongside_plan(Some(&free), 50), Ok(None));
        let hib = Windows { blocker: "hibernated".into(), ..w.clone() };
        assert!(alongside_plan(Some(&hib), 50).unwrap_err().contains("Hızlı Başlatma"));
        assert_eq!(hib.available(), 0);
        let bl = Windows { blocker: "bitlocker".into(), ..w.clone() };
        assert!(alongside_plan(Some(&bl), 50).unwrap_err().contains("BitLocker"));
        assert!(alongside_plan(None, 50).is_err());
        assert!(w.json().contains("\"blocker\":\"\""));
    }

    #[test]
    fn firmware_report() {
        assert_eq!(firmware_json(false, true, true), r#"{"secure_boot":false,"setup_mode":true,"tpm":true}"#);
    }

    #[test]
    fn kernel_command_line() {
        assert_eq!(kernel_cmdline("R", None), "root=UUID=R rw quiet panic=10\n");
        assert_eq!(kernel_cmdline("R", Some("L")), "rd.luks.name=L=cryptroot root=/dev/mapper/cryptroot rw quiet panic=10\n");
        assert!(LOADER_CONF.contains("editor no") && !LOADER_CONF.contains("\ndefault "), "a default would pin a failed entry");
    }

    #[test]
    fn initcpio_hooks() {
        let e = mkinitcpio_dropin(true);
        assert!(e.contains("HOOKS=(base systemd autodetect microcode modconf kms keyboard sd-vconsole block sd-encrypt filesystems fsck osmium-kurtar)"));
        assert!(e.contains("MODULES=(vfat "));
        let p = mkinitcpio_dropin(false);
        assert!(!p.contains("encrypt") && p.contains("filesystems fsck osmium-kurtar)"));
    }

    #[test]
    fn groups_filtered() {
        let g = "root:x:0:\nwheel:x:998:\nvideo:x:986:\ninput:x:97:\nusers:x:985:\n";
        assert_eq!(filter_groups(g), ["users", "wheel", "video", "input"]);
    }

    #[test]
    fn keyboard_layout_in_dots() {
        let lua = "hl.config({\n    input = {\n        kb_layout    = \"tr\",\n        follow_mouse = 1,\n    },\n})\n";
        let out = set_kb_layout(lua, "us", true).unwrap();
        assert!(out.contains("        kb_layout    = \"us\",\n        follow_mouse = 1,\n"), "{out}");
        assert_eq!(set_kb_layout(lua, "tr", true), None, "already set");
        assert_eq!(
            set_kb_layout("input = { kb_layout = 'tr' }\n-- kb_layout = \"tr\"\n", "de", true).unwrap(),
            "input = { kb_layout = 'de' }\n-- kb_layout = \"tr\"\n"
        );
        // A Lua expression is left alone; so are other keys that merely contain the name.
        assert_eq!(set_kb_layout("kb_layout = layout,\nmy_kb_layout = \"tr\"\n", "us", true), None);
        let conf = "input {\r\n    kb_layout = tr\r\n    # kb_layout = xx\n    kb_variant =\n    kb_layout=tr,us\n}";
        assert_eq!(
            set_kb_layout(conf, "fr", false).unwrap(),
            "input {\r\n    kb_layout = fr\r\n    # kb_layout = xx\n    kb_variant =\n    kb_layout=fr\n}"
        );
        assert_eq!(set_kb_layout("exec-once = waybar\n", "us", false), None);
    }

    #[test]
    fn ui_sends_only_keys_the_backend_reads() {
        let qml = include_str!("../../../packages/osmium-dots-ii/overlay/.config/quickshell/ii/osmium-installer.qml");
        let kv = qml.split_once("const kv = {").and_then(|(_, r)| r.split_once('}')).expect("configText() in the QML").0;
        let keys: Vec<&str> = kv
            .split([',', '\n'])
            .filter_map(|pair| pair.split_once(':'))
            .map(|(k, _)| k.trim())
            .collect();
        assert!(keys.len() >= 13, "{keys:?}");
        for key in keys {
            // A value no default can equal: the field changes only if parse_config knows the key.
            let c = parse_config(&format!("{key}=1\n"));
            let d = parse_config("");
            let fields = |c: &Config| format!("{c:?}");
            assert_ne!(fields(&c), fields(&d), "osmium-installer.qml sends `{key}`, which osmium-install ignores");
        }
    }

    #[test]
    fn progress() {
        assert_eq!(parse_pct(" 42\n"), Some(42));
        assert_eq!(parse_pct("100%"), Some(100));
        assert_eq!(parse_pct("Parallel unsquashfs: Using 8 processors"), None);
        assert_eq!(parse_pct("101"), None);
        assert_eq!(parse_pct(""), None);
        assert_eq!(copy_pct(0), 9);
        assert_eq!(copy_pct(100), 72);
    }

    #[test]
    fn plan_is_monotonic() {
        assert!(PLAN.windows(2).all(|w| w[0].1 < w[1].1));
        assert!(PLAN.last().unwrap().1 < 100);
        for id in ["disk", "format", "copy", "config", "boot", "finish"] {
            assert!(PLAN.iter().any(|s| s.0 == id));
        }
    }
}
