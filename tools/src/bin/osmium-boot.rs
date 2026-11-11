//! Boot chain: systemd-boot + signed UKIs, boot counting with automatic rollback, TPM2 unlock,
//! and the rescue screen.
//!
//!   osmium-boot             status (entries, Secure Boot, TPM, last rollback)
//!   osmium-boot kur [--ilk] install /boot/osmium.efi on the ESP (pacman hook, installer)
//!   osmium-boot kontrol     early boot: after a failed update, roll back (osmium-boot-check.service)
//!   osmium-boot ilk         first boot: TPM2 enrollment, drop the installer's key file
//!   osmium-boot tpm         (re)enroll TPM2 unlock, asks for the disk passphrase
//!   osmium-boot guvenli     enroll our Secure Boot keys (firmware in Setup Mode)
//!   osmium-boot kurtar      rescue menu with a QR code of the errors (emergency.service)
//!   osmium-boot ipucu       after login: a one-time notification when the disk could unlock with
//!                           the TPM but does not yet (Secure Boot off), with the steps
//!
//! mkinitcpio builds /boot/osmium.efi (preset `default_uki`). `kur` signs it with sbctl when
//! keys exist and copies it to /efi/EFI/Linux. The first install is `osmium.efi`. Every later
//! one becomes `osmium+3.efi`: systemd-boot counts its tries down, and
//! systemd-bless-boot renames it to `osmium.efi` once `boot-complete.target` is reached
//! (osmium-boot-ok.service waits for the display manager). The entry it replaces, which
//! booted fine, is kept as `osmium-kurtarma.efi`, and the snap-pac snapshot taken before the
//! update is recorded. When the new entry used up its tries, systemd-boot sorts it last and
//! boots the rescue copy; `kontrol` then rolls the root back to the recorded snapshot
//! (`snapper rollback`, the root is the btrfs default subvolume), makes the rescue copy the
//! main entry again and reboots. The rescue copy's old kernel finds no modules on the new root,
//! which is why vfat is in the initramfs and `kontrol` needs nothing else.

use osmium_tools::{Result, exit_on_err, output, run, succeeds};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "kullanım: osmium-boot [kur [--ilk] | kontrol | ilk | tpm | guvenli | kurtar]";
const SRC: &str = "/boot/osmium.efi";
const ESP: &str = "/efi";
const STATE: &str = "/var/lib/osmium/boot";
const RESCUE: &str = "osmium-kurtarma.efi";
const MAIN: &str = "osmium.efi";
const TRIES: u32 = 3;
const EFIVARS: &str = "/sys/firmware/efi/efivars";
const GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const LOADER_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
const SBCTL_KEYS: &str = "/var/lib/sbctl/keys";
const SDBOOT: &str = "/usr/lib/systemd/boot/efi/systemd-bootx64.efi";
/// Bytes of error text in the QR code: a version 25-30 code, still readable from a phone.
const QR_BYTES: usize = 1200;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] | ["durum"] => status(),
        ["kur"] => install_uki(false),
        ["kur", "--ilk"] => install_uki(true),
        ["kontrol"] => check(),
        ["ilk"] => first_boot(),
        ["tpm"] => enroll_tpm(),
        ["guvenli"] => secure_boot(),
        ["kurtar"] => rescue(),
        ["ipucu"] => hint(),
        _ => Err(USAGE.into()),
    };
    exit_on_err("osmium-boot", res);
}

// ---------- pure helpers ----------

#[derive(Debug, PartialEq, Clone, Copy)]
enum Entry {
    /// `osmium.efi`, or `osmium+LEFT[-DONE].efi` while it is being tried.
    Main { tries: Option<(u32, u32)> },
    Rescue,
}

/// One of our files in /efi/EFI/Linux (FAT: any case).
fn parse_entry(name: &str) -> Option<Entry> {
    let name = name.to_ascii_lowercase();
    if name == RESCUE {
        return Some(Entry::Rescue);
    }
    let stem = name.strip_suffix(".efi")?;
    if stem == "osmium" {
        return Some(Entry::Main { tries: None });
    }
    let counter = stem.strip_prefix("osmium+")?;
    let (left, done) = counter.split_once('-').unwrap_or((counter, "0"));
    Some(Entry::Main { tries: Some((left.parse().ok()?, done.parse().ok()?)) })
}

/// A UTF-16LE EFI variable as read from efivarfs (4 attribute bytes first), up to the NUL.
fn efi_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.get(4..).unwrap_or_default().chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// `\EFI\Linux\osmium-kurtarma.efi` from systemd-stub's StubImageIdentifier.
fn is_rescue_image(path: &str) -> bool {
    path.rsplit(['\\', '/']).next().is_some_and(|f| f.eq_ignore_ascii_case(RESCUE))
}

/// Number of the newest `pre` snapshot in `snapper --csvout list --columns number,type`.
fn latest_pre(csv: &str) -> Option<u32> {
    csv.lines().filter_map(|l| l.split_once(',')).filter(|(_, t)| t.trim() == "pre").filter_map(|(n, _)| n.trim().parse().ok()).max()
}

/// The LUKS UUID in a kernel command line (`rd.luks.name=UUID=NAME` or `rd.luks.uuid=UUID`).
fn luks_uuid(cmdline: &str) -> Option<String> {
    cmdline.split_whitespace().find_map(|w| {
        let v = w.strip_prefix("rd.luks.name=").and_then(|v| v.split_once('=').map(|(u, _)| u)).or_else(|| w.strip_prefix("rd.luks.uuid="))?;
        let v = v.strip_prefix("luks-").unwrap_or(v);
        (!v.is_empty() && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then(|| v.to_string())
    })
}

/// `btrfs subvolume get-default /`: the default subvolume's path below the top level.
fn default_subvol_path(text: &str) -> Option<String> {
    text.split_once(" path ").map(|(_, p)| p.trim().to_string()).filter(|p| !p.is_empty())
}

/// Where a subvolume below `@snapshots/` is reachable on the running system.
fn snapshot_mount_path(subvol: &str) -> Option<String> {
    subvol.strip_prefix("@snapshots/").map(|rest| format!("/.snapshots/{rest}"))
}

/// One byte of an efivarfs boolean (SecureBoot, SetupMode): attributes first, then the value.
fn efi_bool(bytes: &[u8]) -> Option<bool> {
    bytes.get(4).map(|b| *b == 1)
}

/// The text behind the QR code: cut at a char boundary so no byte sequence is split.
fn qr_text(kernel: &str, failed: &[String], errors: &str) -> String {
    let mut s = format!("OsmiumOS acilis hatasi\nCekirdek: {kernel}\n");
    if !failed.is_empty() {
        s += &format!("Basarisiz: {}\n", failed.join(" "));
    }
    s += errors;
    let mut end = s.len().min(QR_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

// ---------- system helpers ----------

/// Where the UKIs live: the ESP, or the XBOOTLDR partition at /boot when the installer put
/// OsmiumOS next to Windows (whose ESP is too small), as /etc/osmium/boot.conf says.
fn linux_dir() -> PathBuf {
    let conf = fs::read_to_string("/etc/osmium/boot.conf").unwrap_or_default();
    let part = if conf.lines().any(|l| l.trim() == "xbootldr=/boot") { "/boot" } else { ESP };
    Path::new(part).join("EFI/Linux")
}

fn efivar(name: &str, guid: &str) -> Option<Vec<u8>> {
    fs::read(format!("{EFIVARS}/{name}-{guid}")).ok()
}

fn secure_boot_on() -> bool {
    efivar("SecureBoot", GLOBAL_GUID).and_then(|b| efi_bool(&b)) == Some(true)
        && efivar("SetupMode", GLOBAL_GUID).and_then(|b| efi_bool(&b)) == Some(false)
}

fn setup_mode() -> bool {
    efivar("SetupMode", GLOBAL_GUID).and_then(|b| efi_bool(&b)) == Some(true)
}

fn tpm_present() -> bool {
    Path::new("/sys/class/tpm/tpm0").exists()
}

fn have_keys() -> bool {
    Path::new(SBCTL_KEYS).is_dir()
}

/// Signs in place when our Secure Boot keys exist; a no-op otherwise.
fn sign(path: &Path) -> Result {
    if !have_keys() {
        return Ok(());
    }
    run(Command::new("sbctl").arg("sign").arg(path))
}

/// bootctl installs and updates systemd-boot from the `.signed` copy when there is one.
fn sign_systemd_boot() {
    let signed = format!("{SDBOOT}.signed");
    let newer = |a: &str, b: &str| {
        let m = |p: &str| fs::metadata(p).and_then(|m| m.modified()).ok();
        m(b).is_none() || m(a) > m(b)
    };
    if have_keys() && Path::new(SDBOOT).is_file() && newer(SDBOOT, &signed) {
        warn(run(Command::new("sbctl").args(["sign", "-s", "-o", &signed, SDBOOT])));
    }
}

fn warn(res: Result) {
    if let Err(e) = res {
        eprintln!("osmium-boot: uyarı: {e}");
    }
}

fn state(name: &str) -> PathBuf {
    Path::new(STATE).join(name)
}

fn write_state(name: &str, text: &str) -> Result {
    fs::create_dir_all(STATE).and_then(|()| fs::write(state(name), text)).map_err(|e| format!("{STATE}/{name}: {e}"))
}

fn stamp(path: &Path) -> Result<String> {
    let m = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Ok(format!("{} {t}\n", m.len()))
}

/// Our entries in `dir`: (file name, kind).
fn entries(dir: &Path) -> Vec<(String, Entry)> {
    let names = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
    names.filter_map(|n| parse_entry(&n).map(|k| (n, k))).collect()
}

fn io<T>(res: std::io::Result<T>, what: &Path) -> Result<T> {
    res.map_err(|e| format!("{}: {e}", what.display()))
}

/// Copies through a temporary name, so a power cut never leaves half an entry behind.
fn place(src: &Path, dir: &Path, name: &str) -> Result {
    let tmp = dir.join("osmium.tmp");
    io(fs::copy(src, &tmp), &tmp)?;
    io(fs::File::open(&tmp).and_then(|f| f.sync_all()), &tmp)?;
    io(fs::rename(&tmp, dir.join(name)), &dir.join(name))
}

fn remove_mains(dir: &Path, keep: Option<&str>) -> Result {
    for (name, kind) in entries(dir) {
        if matches!(kind, Entry::Main { .. }) && Some(name.as_str()) != keep {
            io(fs::remove_file(dir.join(&name)), &dir.join(&name))?;
        }
    }
    Ok(())
}

fn root_only() -> Result {
    match output(Command::new("id").arg("-u"))?.trim() {
        "0" => Ok(()),
        _ => Err("root olarak çalıştırın (sudo osmium-boot ...)".into()),
    }
}

fn kernel_cmdline() -> String {
    fs::read_to_string("/etc/kernel/cmdline").unwrap_or_default()
}

fn luks_device() -> Result<String> {
    let uuid = luks_uuid(&kernel_cmdline()).ok_or("disk şifreli değil (/etc/kernel/cmdline içinde rd.luks yok)")?;
    Ok(format!("/dev/disk/by-uuid/{uuid}"))
}

// ---------- commands ----------

fn install_uki(first: bool) -> Result {
    root_only()?;
    let src = Path::new(SRC);
    let dir = &linux_dir();
    // The live ISO and the update rehearsal container have no ESP: nothing to install there.
    if !first && !dir.is_dir() {
        return Ok(());
    }
    if !src.is_file() {
        return Err(format!("{SRC} yok (mkinitcpio -P çalıştı mı?)"));
    }
    sign_systemd_boot();
    let have_main = entries(dir).iter().any(|(_, k)| matches!(k, Entry::Main { .. }));
    let old = fs::read_to_string(state("kurulu")).unwrap_or_default();
    // Signing rewrites the file, so the stamp is taken after it; an unchanged stamp means done.
    if !first && have_main && old == stamp(src)? {
        return Ok(());
    }
    sign(src)?;
    io(fs::create_dir_all(dir), dir)?;
    if first {
        place(src, dir, MAIN)?;
        remove_mains(dir, Some(MAIN))?;
        let _ = fs::remove_file(dir.join(RESCUE));
        let _ = fs::remove_file(state("geri-don"));
    } else {
        // Only an entry that booted fine (no counter left) may become the rescue copy; while
        // an update is still being tried, the rescue copy and the rollback target stay.
        if let Some((good, _)) = entries(dir).into_iter().find(|(_, k)| *k == Entry::Main { tries: None }) {
            io(fs::rename(dir.join(&good), dir.join(RESCUE)), &dir.join(RESCUE))?;
            let csv = output(Command::new("snapper").args(["--no-dbus", "-c", "root", "--csvout", "list", "--columns", "number,type"]));
            match csv.ok().as_deref().and_then(latest_pre) {
                Some(n) => write_state("geri-don", &format!("{n}\n"))?,
                None => {
                    let _ = fs::remove_file(state("geri-don"));
                }
            }
        }
        let name = format!("osmium+{TRIES}.efi");
        place(src, dir, &name)?;
        remove_mains(dir, Some(&name))?;
    }
    write_state("kurulu", &stamp(src)?)
}

/// The image systemd-stub booted from, if it says.
fn booted_image() -> Option<String> {
    efivar("StubImageIdentifier", LOADER_GUID).map(|b| efi_string(&b))
}

fn check() -> Result {
    let dir = &linux_dir();
    if !booted_image().is_some_and(|p| is_rescue_image(&p)) {
        return Ok(());
    }
    // A user who picks the rescue entry by hand while the main one is fine changes nothing.
    if entries(dir).iter().any(|(_, k)| *k == Entry::Main { tries: None }) {
        return Ok(());
    }
    println!("osmium-boot: güncelleme sonrası sistem açılamadı, önceki duruma dönülüyor");
    let target: Option<u32> = fs::read_to_string(state("geri-don")).ok().and_then(|s| s.trim().parse().ok());
    let mut new_root = None;
    if let Some(n) = target {
        run(Command::new("snapper").args(["--no-dbus", "--ambit", "classic", "-c", "root", "rollback", &n.to_string()]))?;
        let def = output(Command::new("btrfs").args(["subvolume", "get-default", "/"]))?;
        new_root = default_subvol_path(&def).and_then(|p| snapshot_mount_path(&p));
    }
    place(&dir.join(RESCUE), dir, MAIN)?;
    remove_mains(dir, Some(MAIN))?;
    io(fs::remove_file(dir.join(RESCUE)), &dir.join(RESCUE))?;
    let note = match target {
        Some(n) => format!("Güncellemeden sonra sistem {TRIES} kez açılamadı; #{n} anlık görüntüsüne dönüldü.\n"),
        None => format!("Güncellemeden sonra sistem {TRIES} kez açılamadı; önceki çekirdeğe dönüldü (anlık görüntü yoktu).\n"),
    };
    let _ = fs::remove_file(state("geri-don"));
    match &new_root {
        // The note belongs on the root that boots next.
        Some(root) => {
            let d = format!("{root}{STATE}");
            warn(fs::create_dir_all(&d).and_then(|()| fs::write(format!("{d}/son-geri-donus"), &note)).map_err(|e| format!("{d}: {e}")));
            run(Command::new("systemctl").arg("reboot"))
        }
        None => write_state("son-geri-donus", &note),
    }
}

fn first_boot() -> Result {
    let key = state("tpm.key");
    if !key.exists() {
        return Ok(());
    }
    let dev = luks_device()?;
    let status = if !tpm_present() {
        TpmStatus::NoTpm
    } else if !secure_boot_on() {
        println!("osmium-boot: Güvenli Önyükleme kapalı: disk parolayla açılmaya devam edecek");
        TpmStatus::SecureBootOff
    } else {
        match run(Command::new("systemd-cryptenroll")
            .arg(format!("--unlock-key-file={}", key.display()))
            .args(["--tpm2-device=auto", "--tpm2-pcrs=7", &dev]))
        {
            Ok(()) => TpmStatus::Enrolled,
            Err(e) => {
                warn(Err(e));
                TpmStatus::SecureBootOff
            }
        }
    };
    warn(write_state("tpm-durum", status.as_str()));
    // The installer's key file has done its job; the slot it opens goes with it.
    warn(run(Command::new("cryptsetup").arg("luksRemoveKey").arg(&dev).arg(&key)));
    io(fs::remove_file(&key), &key)
}

/// What the first boot found, in /var/lib/osmium/boot/tpm-durum (world-readable: the user's
/// session reads it without root, which `cryptsetup luksDump` would need).
#[derive(Debug, PartialEq, Clone, Copy)]
enum TpmStatus {
    Enrolled,
    SecureBootOff,
    NoTpm,
}

impl TpmStatus {
    fn as_str(self) -> &'static str {
        match self {
            TpmStatus::Enrolled => "kayitli",
            TpmStatus::SecureBootOff => "guvenli-onyukleme-kapali",
            TpmStatus::NoTpm => "tpm-yok",
        }
    }

    fn parse(s: &str) -> Option<TpmStatus> {
        [TpmStatus::Enrolled, TpmStatus::SecureBootOff, TpmStatus::NoTpm].into_iter().find(|t| t.as_str() == s.trim())
    }
}

/// The steps to TPM unlock when Secure Boot is off, as shown after login and by `ipucu`.
const TPM_STEPS: &str = "Disk her açılışta parola soruyor. Bu bilgisayarda parolasız açılış (TPM) için:\n\
    1. sudo osmium-boot guvenli  (ne yapacağınızı adım adım yazar)\n\
    2. UEFI ayarlarında Secure Boot'u \"Setup Mode\"a alın, komutu tekrarlayın\n\
    3. Yeniden başlatıp Secure Boot'u açın, sonra: sudo osmium-boot tpm";

/// One notification per user when the disk could unlock with the TPM but Secure Boot is off.
fn hint() -> Result {
    let status = fs::read_to_string(state("tpm-durum")).ok().and_then(|s| TpmStatus::parse(&s));
    if status != Some(TpmStatus::SecureBootOff) || secure_boot_on() {
        return Ok(());
    }
    let marker = osmium_tools::state_home()?.join("osmium/tpm-ipucu");
    if marker.exists() {
        return Ok(());
    }
    // The notification daemon starts with the desktop.
    for _ in 0..60 {
        if succeeds(Command::new("hyprctl").arg("version")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    std::thread::sleep(std::time::Duration::from_secs(10));
    println!("{TPM_STEPS}");
    if succeeds(Command::new("notify-send").args(["-a", "OsmiumOS", "-t", "0", "Parolasız disk açılışı", TPM_STEPS])) {
        let _ = fs::create_dir_all(marker.parent().unwrap_or(&marker));
        let _ = fs::write(&marker, "");
    }
    Ok(())
}

fn enroll_tpm() -> Result {
    root_only()?;
    let dev = luks_device()?;
    if !tpm_present() {
        return Err("bu makinede TPM bulunamadı".into());
    }
    if !secure_boot_on() {
        return Err("Güvenli Önyükleme açık değil. TPM ile açılış yalnızca Güvenli Önyükleme açıkken güvenli: \
                    önce `sudo osmium-boot guvenli`"
            .into());
    }
    println!("Disk parolanız sorulacak; ardından disk bu makinede parolasız açılır (TPM, PCR 7).");
    run(Command::new("systemd-cryptenroll").args(["--wipe-slot=tpm2", "--tpm2-device=auto", "--tpm2-pcrs=7", &dev]))?;
    warn(write_state("tpm-durum", TpmStatus::Enrolled.as_str()));
    println!("Tamam. Güvenli Önyükleme ayarları değişirse disk yine parola ister; o zaman bu komutu tekrar çalıştırın.");
    Ok(())
}

fn secure_boot() -> Result {
    root_only()?;
    if secure_boot_on() && have_keys() {
        println!("Güvenli Önyükleme açık ve OsmiumOS anahtarlarıyla çalışıyor.");
        return Ok(());
    }
    if !have_keys() {
        run(Command::new("sbctl").arg("create-keys"))?;
    }
    if !setup_mode() {
        println!(
            "Ürün yazılımı (UEFI) Kurulum Modu'nda değil, anahtarlar yüklenemiyor.\n\
             1. Bilgisayarı yeniden başlatıp UEFI ayarlarına girin (genelde F2, Del ya da F10).\n\
             2. Secure Boot bölümünde \"Reset to Setup Mode\" ya da \"Clear Secure Boot keys\" seçin.\n\
             3. OsmiumOS'ta tekrar: sudo osmium-boot guvenli"
        );
        return Ok(());
    }
    // Microsoft's keys stay: GPU and NIC option ROMs are signed with them.
    run(Command::new("sbctl").args(["enroll-keys", "--microsoft"]))?;
    sign_systemd_boot();
    warn(run(Command::new("bootctl").args(["update", "--graceful"])));
    let mut files: Vec<PathBuf> = ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"].iter().map(|f| Path::new(ESP).join(f)).collect();
    let dir = linux_dir();
    files.extend(entries(&dir).into_iter().map(|(n, _)| dir.join(n)));
    for f in files.iter().filter(|f| f.is_file()) {
        sign(f)?;
    }
    if Path::new(SRC).is_file() {
        sign(Path::new(SRC))?;
        write_state("kurulu", &stamp(Path::new(SRC))?)?;
    }
    println!("Anahtarlar yüklendi. Yeniden başlatın; UEFI ayarlarında Secure Boot açık değilse açın.");
    if luks_uuid(&kernel_cmdline()).is_some() && tpm_present() {
        println!("Sonra diski parolasız açmak için: sudo osmium-boot tpm");
    }
    Ok(())
}

fn status() -> Result {
    let dir = &linux_dir();
    let mut list = entries(dir);
    list.sort_by(|a, b| a.0.cmp(&b.0));
    println!("Açılış girdileri ({}):", dir.display());
    if list.is_empty() {
        println!("  (yok ya da bölüm bağlı değil)");
    }
    for (name, kind) in &list {
        let what = match kind {
            Entry::Main { tries: None } => "çalıştığı doğrulandı".to_string(),
            Entry::Main { tries: Some((0, _)) } => "açılamadı, denemeler bitti".to_string(),
            Entry::Main { tries: Some((left, _)) } => format!("deneniyor, {left} hak kaldı"),
            Entry::Rescue => "kurtarma kopyası (önceki çalışan çekirdek)".to_string(),
        };
        println!("  {name}: {what}");
    }
    if let Some(n) = fs::read_to_string(state("geri-don")).ok().map(|s| s.trim().to_string()) {
        println!("Güncelleme açılmazsa dönülecek anlık görüntü: #{n}");
    }
    println!(
        "Güvenli Önyükleme: {}",
        match (secure_boot_on(), setup_mode(), have_keys()) {
            (true, _, _) => "açık",
            (false, true, _) => "kapalı, ürün yazılımı Kurulum Modu'nda (sudo osmium-boot guvenli)",
            (false, false, true) => "kapalı (anahtarlar hazır, UEFI ayarlarından açın)",
            (false, false, false) => "kapalı",
        }
    );
    let tpm = match (tpm_present(), luks_device()) {
        (false, _) => "yok".to_string(),
        (true, Err(_)) => "var (disk şifreli değil)".to_string(),
        (true, Ok(dev)) => {
            let dump = output(Command::new("cryptsetup").args(["luksDump", &dev])).unwrap_or_default();
            if dump.contains("systemd-tpm2") { "disk TPM ile açılıyor" } else { "var, disk parolayla açılıyor (sudo osmium-boot tpm)" }.to_string()
        }
    };
    println!("TPM: {tpm}");
    if let Ok(note) = fs::read_to_string(state("son-geri-donus")) {
        print!("Son geri dönüş: {note}");
    }
    Ok(())
}

// ---------- rescue ----------

fn failed_units() -> Vec<String> {
    let text = output(Command::new("systemctl").args(["--failed", "--no-legend", "--plain"])).unwrap_or_default();
    text.lines().filter_map(|l| l.split_whitespace().find(|w| w.contains('.')).map(String::from)).collect()
}

fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn show_qr(text: &str) {
    let child = Command::new("qrencode").args(["-t", "ANSIUTF8", "-m", "2", "-l", "L"]).stdin(Stdio::piped()).spawn();
    if let Ok(mut c) = child {
        if let Some(mut stdin) = c.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = c.wait();
    }
}

/// Rolls the root back to snapshot `n` and boots that root's own kernel next.
fn rollback_to(n: u32) -> Result {
    succeeds(Command::new("mount").args(["-o", "remount,rw", "/"]));
    for m in ["/.snapshots", ESP] {
        succeeds(Command::new("mount").arg(m));
    }
    run(Command::new("snapper").args(["--no-dbus", "--ambit", "classic", "-c", "root", "rollback", &n.to_string()]))?;
    let def = output(Command::new("btrfs").args(["subvolume", "get-default", "/"]))?;
    let root = default_subvol_path(&def).and_then(|p| snapshot_mount_path(&p)).ok_or("yeni kök alt birimi bulunamadı")?;
    let uki = PathBuf::from(format!("{root}{SRC}"));
    if uki.is_file() {
        sign(&uki)?;
        let dir = &linux_dir();
        place(&uki, dir, MAIN)?;
        remove_mains(dir, Some(MAIN))?;
        let _ = fs::remove_file(dir.join(RESCUE));
    }
    let d = format!("{root}{STATE}");
    let _ = fs::create_dir_all(&d);
    let _ = fs::write(format!("{d}/son-geri-donus"), format!("Kurtarma ekranından #{n} anlık görüntüsüne dönüldü.\n"));
    Ok(())
}

/// The rescue menu's choices; the initramfs has no root file system, so no snapshots there.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Choice {
    Rollback,
    PreviousKernel,
    Shell,
    Reboot,
}

fn menu(in_initrd: bool) -> Vec<(Choice, &'static str)> {
    let mut v = Vec::new();
    if !in_initrd {
        v.push((Choice::Rollback, "Önceki anlık görüntüye dön (son güncellemeden önceki hal)"));
    }
    v.push((Choice::PreviousKernel, "Önceki çekirdekle bir kez aç"));
    v.push((Choice::Shell, "Komut satırı (root parolası ister)"));
    v.push((Choice::Reboot, "Yeniden başlat"));
    v
}

/// efivarfs contents of a systemd-boot string variable: attributes (non-volatile, boot and
/// runtime access), then UTF-16LE with a NUL.
fn efi_var_bytes(value: &str) -> Vec<u8> {
    let mut v = vec![7, 0, 0, 0];
    v.extend(value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    v
}

/// Next boot only: systemd-boot starts the rescue copy (the kernel before the last update).
/// Works from the initramfs too, where the root (and with it the ESP) may be unreachable.
fn boot_previous_once() -> Result {
    let path = format!("{EFIVARS}/LoaderEntryOneShot-{LOADER_GUID}");
    // efivarfs marks existing variables immutable.
    succeeds(Command::new("chattr").args(["-i", &path]));
    fs::write(&path, efi_var_bytes(RESCUE)).map_err(|e| format!("{path}: {e}"))
}

fn rescue() -> Result {
    let in_initrd = Path::new("/etc/initrd-release").exists();
    let kernel = output(Command::new("uname").arg("-r")).unwrap_or_default();
    let failed = failed_units();
    let errors = output(Command::new("journalctl").args(["-b", "-p", "err", "-n", "25", "--no-pager", "-o", "cat"])).unwrap_or_default();
    let summary = qr_text(kernel.trim(), &failed, &errors);
    let choices = menu(in_initrd);
    loop {
        println!("\n=== OsmiumOS kurtarma ===");
        println!("{}\n", if in_initrd { "Sistem diski açılamadı ya da bulunamadı." } else { "Sistem açılırken bir sorun oldu." });
        println!("{summary}\n");
        println!("Bu kod hata özetini içerir: telefonla okutup yardım isterken paylaşabilirsiniz.");
        show_qr(&summary);
        let target: Option<u32> = if in_initrd {
            None
        } else {
            let csv = output(Command::new("snapper").args(["--no-dbus", "-c", "root", "--csvout", "list", "--columns", "number,type"]))
                .unwrap_or_default();
            fs::read_to_string(state("geri-don")).ok().and_then(|s| s.trim().parse().ok()).or_else(|| latest_pre(&csv))
        };
        println!();
        for (i, (c, text)) in choices.iter().enumerate() {
            let extra = match (c, target) {
                (Choice::Rollback, Some(n)) => format!(" (#{n})"),
                (Choice::Rollback, None) => " (anlık görüntü yok)".into(),
                _ => String::new(),
            };
            println!("{}) {text}{extra}", i + 1);
        }
        print!("Seçiminiz: ");
        let _ = std::io::stdout().flush();
        let Some(line) = read_line() else { return Ok(()) };
        let picked = line.parse::<usize>().ok().and_then(|n| choices.get(n.wrapping_sub(1))).map(|(c, _)| *c);
        match picked {
            Some(Choice::Rollback) => match target.ok_or_else(|| "anlık görüntü yok".to_string()).and_then(rollback_to) {
                Ok(()) => {
                    println!("Geri dönüldü, yeniden başlatılıyor.");
                    return run(Command::new("systemctl").arg("reboot"));
                }
                Err(e) => println!("Geri dönülemedi: {e}"),
            },
            Some(Choice::PreviousKernel) => match boot_previous_once() {
                Ok(()) => {
                    println!("Bir sonraki açılış önceki çekirdekle; yeniden başlatılıyor.");
                    return run(Command::new("systemctl").arg("reboot"));
                }
                Err(e) => println!("Ayarlanamadı: {e}"),
            },
            Some(Choice::Shell) => {
                let shell = Command::new("/usr/lib/systemd/systemd-sulogin-shell").arg("emergency").status();
                if shell.is_err() {
                    let _ = Command::new("/usr/bin/sulogin").status();
                }
            }
            Some(Choice::Reboot) => return run(Command::new("systemctl").arg("reboot")),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescue_menu() {
        let main: Vec<Choice> = menu(false).into_iter().map(|(c, _)| c).collect();
        assert_eq!(main, [Choice::Rollback, Choice::PreviousKernel, Choice::Shell, Choice::Reboot]);
        let initrd: Vec<Choice> = menu(true).into_iter().map(|(c, _)| c).collect();
        assert_eq!(initrd, [Choice::PreviousKernel, Choice::Shell, Choice::Reboot], "no snapshots without a root");
        let b = efi_var_bytes("osmium-kurtarma.efi");
        assert_eq!(&b[..6], &[7, 0, 0, 0, b'o', 0]);
        assert_eq!(efi_string(&b), "osmium-kurtarma.efi");
        assert!(b.ends_with(&[0, 0]));
    }

    #[test]
    fn tpm_status_round_trip() {
        for t in [TpmStatus::Enrolled, TpmStatus::SecureBootOff, TpmStatus::NoTpm] {
            assert_eq!(TpmStatus::parse(&format!("{}\n", t.as_str())), Some(t));
        }
        assert_eq!(TpmStatus::parse("?"), None);
        assert!(TPM_STEPS.contains("osmium-boot guvenli") && TPM_STEPS.contains("osmium-boot tpm"));
    }

    #[test]
    fn entry_names() {
        assert_eq!(parse_entry("osmium.efi"), Some(Entry::Main { tries: None }));
        assert_eq!(parse_entry("OSMIUM.EFI"), Some(Entry::Main { tries: None }));
        assert_eq!(parse_entry("osmium+3.efi"), Some(Entry::Main { tries: Some((3, 0)) }));
        assert_eq!(parse_entry("osmium+1-2.efi"), Some(Entry::Main { tries: Some((1, 2)) }));
        assert_eq!(parse_entry("osmium+0-3.efi"), Some(Entry::Main { tries: Some((0, 3)) }));
        assert_eq!(parse_entry("osmium-kurtarma.efi"), Some(Entry::Rescue));
        for other in ["osmium.tmp", "arch-linux.efi", "osmium+x.efi", "osmium+.efi", "osmium-zen.efi"] {
            assert_eq!(parse_entry(other), None, "{other}");
        }
    }

    fn efivar_bytes(s: &str) -> Vec<u8> {
        let mut v = vec![6, 0, 0, 0];
        v.extend(s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        v
    }

    #[test]
    fn reads_the_booted_image() {
        let id = efi_string(&efivar_bytes("\\EFI\\Linux\\osmium-kurtarma.efi"));
        assert_eq!(id, "\\EFI\\Linux\\osmium-kurtarma.efi");
        assert!(is_rescue_image(&id));
        assert!(is_rescue_image("\\EFI\\LINUX\\OSMIUM-KURTARMA.EFI"));
        assert!(!is_rescue_image(&efi_string(&efivar_bytes("\\EFI\\Linux\\osmium+2-1.efi"))));
        assert_eq!(efi_string(&[1, 2]), "");
        assert_eq!(efi_bool(&[7, 0, 0, 0, 1]), Some(true));
        assert_eq!(efi_bool(&[7, 0, 0, 0, 0]), Some(false));
        assert_eq!(efi_bool(&[7, 0]), None);
    }

    #[test]
    fn finds_the_pre_update_snapshot() {
        let csv = "number,type\n0,single\n1,single\n2,pre\n3,post\n7,pre\n8,post\n";
        assert_eq!(latest_pre(csv), Some(7));
        assert_eq!(latest_pre("number,type\n0,single\n"), None);
    }

    #[test]
    fn finds_the_luks_device() {
        let u = "0a1b2c3d-1111-2222-3333-444455556666";
        assert_eq!(luks_uuid(&format!("rd.luks.name={u}=cryptroot root=/dev/mapper/cryptroot rw")).as_deref(), Some(u));
        assert_eq!(luks_uuid(&format!("quiet rd.luks.uuid=luks-{u}")).as_deref(), Some(u));
        assert_eq!(luks_uuid("root=UUID=abc rw quiet"), None);
        assert_eq!(luks_uuid("rd.luks.name=../x=y"), None);
    }

    #[test]
    fn locates_the_rolled_back_root() {
        let def = "ID 312 gen 1450 top level 260 path @snapshots/12/snapshot\n";
        assert_eq!(default_subvol_path(def).as_deref(), Some("@snapshots/12/snapshot"));
        assert_eq!(snapshot_mount_path("@snapshots/12/snapshot").as_deref(), Some("/.snapshots/12/snapshot"));
        assert_eq!(snapshot_mount_path("@"), None);
        assert_eq!(default_subvol_path("ID 5 (FS_TREE)\n"), None);
    }

    #[test]
    fn qr_text_is_bounded() {
        let t = qr_text("6.12.1-zen1-1-zen", &["greetd.service".into()], "hata\n");
        assert!(t.contains("Basarisiz: greetd.service") && t.ends_with("hata\n"));
        let long = "ş".repeat(2000);
        let t = qr_text("k", &[], &long);
        assert!(t.len() <= QR_BYTES && t.is_char_boundary(t.len()));
    }
}
