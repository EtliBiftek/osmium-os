//! One-command health check and repair.
//!
//!   osmium-doctor         run every check, one line per finding (sorun / uyarı / tamam)
//!   osmium-doctor onar    offer a fix for each fixable finding, one `evet` each
//!   osmium-doctor --ai    let osmium-ai explain the findings
//!
//! Exits 0 when nothing is `sorun`, 1 otherwise, 2 on a usage error. Run it as
//! your user, not through sudo: it also checks your `systemctl --user` units and
//! calls sudo itself for the root steps of `onar`.

use osmium_tools::{Result, run, succeeds};
use std::collections::BTreeMap;
use std::io::{BufRead, ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const DB_LOCK: &str = "/var/lib/pacman/db.lck";
const DISK_LIMIT: u32 = 90;
const JOURNAL_LIMIT: u64 = 1 << 30;
const AI_CAP: usize = 6000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Sev {
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, PartialEq)]
enum Fix {
    StaleLock,
    Units { user: bool, names: Vec<String> },
    Orphans(Vec<String>),
    Journal,
    PkgCache,
}

#[derive(Debug, PartialEq)]
struct Finding {
    sev: Sev,
    text: String,
    fix: Option<Fix>,
}

fn finding(sev: Sev, text: impl Into<String>) -> Finding {
    Finding { sev, text: text.into(), fix: None }
}

fn unknown(what: &str) -> Finding {
    finding(Sev::Warn, format!("{what}: denetlenemedi"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map_or("", String::as_str);
    // Before the checks: a typo must not cost a full scan.
    if args.len() > 1 || !matches!(mode, "" | "onar" | "--ai") {
        eprintln!("kullanım: osmium-doctor [onar | --ai]");
        std::process::exit(2);
    }
    let mut findings = collect();
    print!("{}", report(&findings));
    match mode {
        "onar" => {
            if repair(&findings) {
                println!("\nYeniden denetleniyor...");
                findings = collect();
                print!("{}", report(&findings));
            }
        }
        "--ai" => {
            let prompt = format!(
                "Aşağıdaki OsmiumOS sağlık raporunu açıkla; önce en önemli sorunu ve çözümünü söyle.\n\n{}",
                report(&findings).chars().take(AI_CAP).collect::<String>()
            );
            println!();
            if let Err(e) = run(Command::new("osmium-ai").args(["ask", &prompt])) {
                eprintln!("osmium-doctor: {e}");
            }
        }
        _ => {}
    }
    std::process::exit(findings.iter().any(|f| f.sev == Sev::Bad) as i32);
}

fn report(findings: &[Finding]) -> String {
    let mut s = String::new();
    for f in findings {
        let label = match f.sev {
            Sev::Ok => "tamam",
            Sev::Warn => "uyarı",
            Sev::Bad => "sorun",
        };
        s += &format!("{label}  {}\n", f.text);
    }
    let count = |sev| findings.iter().filter(|f| f.sev == sev).count();
    s + &format!("\n{} sorun, {} uyarı\n", count(Sev::Bad), count(Sev::Warn))
}

// ---------- runners ----------

fn stdout_of(argv: &[&str], any_exit: bool) -> Option<String> {
    // LC_ALL=C: several parsers below read English words (df header, journalctl, paccache).
    let out = Command::new(argv[0]).args(&argv[1..]).env("LC_ALL", "C").stderr(Stdio::null()).output().ok()?;
    (any_exit || out.status.success()).then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Stdout of a successful run.
fn capture(argv: &[&str]) -> Option<String> {
    stdout_of(argv, false)
}

/// Stdout whatever the exit code: `pacman -Qk` and `-Qtd` use the code as the answer.
fn capture_any(argv: &[&str]) -> Option<String> {
    stdout_of(argv, true)
}

/// For read-only commands that may need root: retried with `sudo -n`, which
/// only works while sudo credentials are cached and never prompts.
fn capture_root(argv: &[&str]) -> Option<String> {
    capture(argv).or_else(|| capture(&[&["sudo", "-n"], argv].concat()))
}

/// PackageKit (Discover, GNOME Software) and pamac take the lock through libalpm, without a pacman process.
const LOCK_HOLDERS: &str = "pacman|packagekitd|pamac-daemon";

fn pacman_running() -> bool {
    succeeds(Command::new("pgrep").args(["-x", LOCK_HOLDERS]))
}

/// The checks are independent and mostly wait on a subprocess (`pacman -Qk` alone takes
/// seconds), so they run side by side; the report keeps this order.
const CHECKS: &[fn() -> Vec<Finding>] = &[
    check_units,
    check_disks,
    check_btrfs,
    check_lock,
    check_package_files,
    check_orphans,
    check_journal,
    check_pacnew,
    check_cache,
    check_snapper,
    check_memory,
];

fn collect() -> Vec<Finding> {
    std::thread::scope(|s| {
        let running: Vec<_> = CHECKS.iter().map(|check| s.spawn(check)).collect();
        running.into_iter().flat_map(|h| h.join().unwrap_or_else(|_| vec![unknown("iç hata")])).collect()
    })
}

fn check_units() -> Vec<Finding> {
    [false, true]
        .into_iter()
        .map(|user| {
            let mut argv = vec!["systemctl", "--failed", "--no-legend", "--plain", "--no-pager"];
            if user {
                argv.insert(1, "--user");
            }
            match capture(&argv) {
                Some(t) => units_finding(user, parse_units(&t)),
                None => unknown(if user { "kullanıcı birimleri" } else { "sistem birimleri" }),
            }
        })
        .collect()
}

fn check_disks() -> Vec<Finding> {
    match capture(&["df", "--output=pcent,target", "/", "/home"]) {
        Some(t) => parse_df(&t).into_iter().map(|(target, pct)| disk_finding(&target, pct)).collect(),
        None => vec![unknown("disk doluluğu")],
    }
}

fn check_btrfs() -> Vec<Finding> {
    vec![match capture_root(&["btrfs", "device", "stats", "/"]) {
        Some(t) => btrfs_finding(&parse_btrfs_stats(&t)),
        None => unknown("btrfs aygıt hataları"),
    }]
}

fn check_lock() -> Vec<Finding> {
    vec![lock_finding(Path::new(DB_LOCK).exists(), pacman_running())]
}

fn check_package_files() -> Vec<Finding> {
    vec![match capture_any(&["pacman", "-Qkq"]) {
        Some(t) => {
            // Without root, files in root-only directories look missing: keep the ones that really are.
            let gone = |l: &&str| {
                l.split_once(' ').is_some_and(|(_, p)| {
                    Path::new(p).symlink_metadata().is_err_and(|e| e.kind() == ErrorKind::NotFound)
                })
            };
            missing_finding(&t.lines().filter(gone).collect::<Vec<_>>().join("\n"))
        }
        None => unknown("paket dosyaları"),
    }]
}

fn check_orphans() -> Vec<Finding> {
    vec![match capture_any(&["pacman", "-Qtdq"]) {
        Some(t) => orphans_finding(parse_names(&t)),
        None => unknown("sahipsiz paketler"),
    }]
}

fn check_journal() -> Vec<Finding> {
    let mut f = vec![match capture(&["journalctl", "-b", "-p", "err", "-q", "--no-pager", "-o", "cat"]) {
        Some(t) => journal_finding(&t),
        None => unknown("günlük hataları"),
    }];
    if let Some(bytes) = capture(&["journalctl", "--disk-usage"]).and_then(|t| journal_usage(&t))
        && bytes > JOURNAL_LIMIT
    {
        f.push(Finding {
            sev: Sev::Warn,
            text: format!("sistem günlüğü {:.1} GiB yer kaplıyor", bytes as f64 / JOURNAL_LIMIT as f64),
            fix: Some(Fix::Journal),
        });
    }
    f
}

fn check_pacnew() -> Vec<Finding> {
    vec![match capture(&["pacdiff", "-o"]) {
        Some(t) => pacnew_finding(&t),
        None => unknown(".pacnew dosyaları"),
    }]
}

fn check_cache() -> Vec<Finding> {
    let n = capture_any(&["paccache", "-dk2"]).map_or(0, |t| cache_candidates(&t));
    if n == 0 {
        return Vec::new();
    }
    vec![Finding {
        sev: Sev::Warn,
        text: format!("paket önbelleğinde silinebilecek {n} eski paket var"),
        fix: Some(Fix::PkgCache),
    }]
}

fn check_snapper() -> Vec<Finding> {
    vec![match capture_root(&["snapper", "--csvout", "-c", "root", "list", "--columns", "number"]) {
        Some(t) => snapper_finding(
            snapper_count(&t),
            succeeds(Command::new("systemctl").args(["is-active", "snapper-timeline.timer"])),
        ),
        None => finding(Sev::Warn, "snapper: anlık görüntüler okunamadı (önce `sudo -v`, sonra tekrar deneyin)"),
    }]
}

fn check_memory() -> Vec<Finding> {
    vec![match std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| meminfo(&t)) {
        Some((total, avail, swap)) => mem_finding(total, avail, swap),
        None => unknown("bellek"),
    }]
}

// ---------- parsers and decisions ----------

fn valid_unit(s: &str) -> bool {
    s.contains('.')
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\'))
}

fn valid_pkg(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '+'))
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-'))
}

/// Unit names from `systemctl --failed --no-legend`; older systemd prefixes a bullet.
fn parse_units(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().find(|t| t.is_ascii()))
        .filter(|u| valid_unit(u))
        .map(String::from)
        .collect()
}

fn units_finding(user: bool, names: Vec<String>) -> Finding {
    let scope = if user { "kullanıcı" } else { "sistem" };
    if names.is_empty() {
        return finding(Sev::Ok, format!("başarısız {scope} birimi yok"));
    }
    Finding {
        sev: Sev::Bad,
        text: format!("{} başarısız {scope} birimi: {}", names.len(), names.join(", ")),
        fix: Some(Fix::Units { user, names }),
    }
}

/// `(mount point, use%)` from `df --output=pcent,target`, one entry per mount point.
fn parse_df(text: &str) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pct), Some(target)) = (it.next(), it.next()) else { continue };
        let Some(pct) = pct.strip_suffix('%').and_then(|p| p.parse().ok()) else { continue };
        if !v.iter().any(|(t, _)| t == target) {
            v.push((target.into(), pct));
        }
    }
    v
}

fn disk_finding(target: &str, pct: u32) -> Finding {
    if pct > DISK_LIMIT {
        finding(Sev::Bad, format!("{target} diski %{pct} dolu"))
    } else {
        finding(Sev::Ok, format!("{target} diski %{pct} dolu"))
    }
}

/// Non-zero counters from `btrfs device stats`: `[/dev/sda2].write_io_errs    3`.
fn parse_btrfs_stats(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, n) = (it.next()?, it.next()?.parse::<u64>().ok()?);
            (n > 0).then(|| (name.to_string(), n))
        })
        .collect()
}

fn btrfs_finding(errors: &[(String, u64)]) -> Finding {
    if errors.is_empty() {
        return finding(Sev::Ok, "btrfs aygıt hatası yok");
    }
    let list: Vec<String> = errors.iter().map(|(k, n)| format!("{k}={n}")).collect();
    finding(Sev::Bad, format!("btrfs aygıt hataları (disk bozuluyor olabilir, yedek al): {}", list.join(", ")))
}

fn lock_finding(lock_exists: bool, pacman_running: bool) -> Finding {
    match (lock_exists, pacman_running) {
        (true, false) => Finding {
            sev: Sev::Bad,
            text: format!("pacman çalışmıyor ama kilit dosyası duruyor: {DB_LOCK}"),
            fix: Some(Fix::StaleLock),
        },
        (true, true) => finding(Sev::Ok, "pacman şu an çalışıyor (kilit normal)"),
        (false, _) => finding(Sev::Ok, "pacman kilidi yok"),
    }
}

/// `pacman -Qkq` prints `<package> <missing path>` per missing file.
fn missing_finding(text: &str) -> Finding {
    let mut pkgs: Vec<&str> = Vec::new();
    let mut files = 0;
    for pkg in text.lines().filter_map(|l| l.split_whitespace().next()) {
        files += 1;
        if !pkgs.contains(&pkg) {
            pkgs.push(pkg);
        }
    }
    if files == 0 {
        return finding(Sev::Ok, "paket dosyaları eksiksiz");
    }
    let more = if pkgs.len() > 5 { ", ..." } else { "" };
    let first = pkgs.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    finding(
        Sev::Bad,
        format!(
            "{} pakette {files} eksik dosya: {first}{more} (düzeltmek için: sudo pacman -S <paket>)",
            pkgs.len()
        ),
    )
}

fn parse_names(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|n| valid_pkg(n)).map(String::from).collect()
}

fn orphans_finding(names: Vec<String>) -> Finding {
    if names.is_empty() {
        return finding(Sev::Ok, "sahipsiz paket yok");
    }
    let more = if names.len() > 8 { ", ..." } else { "" };
    Finding {
        sev: Sev::Warn,
        text: format!("{} sahipsiz paket: {}{more}", names.len(), names.iter().take(8).cloned().collect::<Vec<_>>().join(", ")),
        fix: Some(Fix::Orphans(names)),
    }
}

/// Total line count and the `n` most frequent messages. Digits are folded so
/// that pids, addresses and counters do not split one message into many.
fn top_messages(text: &str, n: usize) -> (usize, Vec<(usize, String)>) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        total += 1;
        let mut key = String::new();
        for c in line.chars() {
            if !c.is_ascii_digit() {
                key.push(c);
            } else if !key.ends_with('#') {
                key.push('#');
            }
        }
        *counts.entry(key).or_default() += 1;
    }
    let mut top: Vec<(usize, String)> = counts.into_iter().map(|(k, c)| (c, k)).collect();
    top.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    top.truncate(n);
    (total, top)
}

fn journal_finding(text: &str) -> Finding {
    let (total, top) = top_messages(text, 5);
    if total == 0 {
        return finding(Sev::Ok, "bu açılışta günlükte hata yok");
    }
    let mut s = format!("bu açılışta günlükte {total} hata; en sık görülenler:");
    for (count, msg) in top {
        s += &format!("\n         {count}x {}", msg.chars().take(110).collect::<String>());
    }
    finding(Sev::Warn, s)
}

/// Bytes from `journalctl --disk-usage`: "... take up 1.2G in the file system."
fn journal_usage(text: &str) -> Option<u64> {
    text.split_whitespace().find_map(|tok| {
        let unit = tok.chars().last()?;
        let shift = match unit {
            'B' => 0,
            'K' => 10,
            'M' => 20,
            'G' => 30,
            'T' => 40,
            _ => return None,
        };
        let n: f64 = tok[..tok.len() - 1].parse().ok()?;
        Some((n * (1u64 << shift) as f64) as u64)
    })
}

fn pacnew_finding(text: &str) -> Finding {
    let files: Vec<&str> = text.lines().map(str::trim).filter(|l| l.ends_with(".pacnew")).collect();
    if files.is_empty() {
        return finding(Sev::Ok, "bekleyen .pacnew dosyası yok");
    }
    let more = if files.len() > 5 { ", ..." } else { "" };
    finding(
        Sev::Warn,
        format!(
            "{} bekleyen .pacnew dosyası: {}{more} (birleştirmek için: sudo pacdiff)",
            files.len(),
            files.iter().take(5).copied().collect::<Vec<_>>().join(", ")
        ),
    )
}

/// Candidate count from `paccache -d`: "==> finished dry run: 12 candidates (...)".
fn cache_candidates(text: &str) -> u32 {
    let toks: Vec<&str> = text.split_whitespace().collect();
    toks.windows(2).find(|w| w[1].starts_with("candidate")).and_then(|w| w[0].parse().ok()).unwrap_or(0)
}

/// Snapshots in `snapper --csvout list --columns number`, without the live system (0).
fn snapper_count(text: &str) -> usize {
    text.lines().filter(|l| l.trim().parse::<u32>().is_ok_and(|n| n > 0)).count()
}

fn snapper_finding(count: usize, timer_active: bool) -> Finding {
    match (count, timer_active) {
        (0, _) => finding(Sev::Warn, "hiç snapper anlık görüntüsü yok: geri dönüş noktan yok"),
        (n, false) => finding(
            Sev::Warn,
            format!("{n} anlık görüntü var ama snapper-timeline.timer etkin değil (sudo systemctl enable --now snapper-timeline.timer)"),
        ),
        (n, true) => finding(Sev::Ok, format!("{n} anlık görüntü, zaman çizelgesi etkin")),
    }
}

/// `(MemTotal, MemAvailable, SwapTotal)` in kB.
fn meminfo(text: &str) -> Option<(u64, u64, u64)> {
    let get = |key: &str| {
        text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<u64>().ok())
    };
    Some((get("MemTotal")?, get("MemAvailable")?, get("SwapTotal")?))
}

fn mem_finding(total: u64, avail: u64, swap: u64) -> Finding {
    let mib = avail / 1024;
    if avail * 10 < total && swap == 0 {
        finding(Sev::Warn, format!("boş bellek az ({mib} MiB) ve takas/zram yok: sistem donabilir"))
    } else {
        finding(Sev::Ok, format!("kullanılabilir bellek {mib} MiB"))
    }
}

// ---------- repair ----------

/// The exact commands a fix runs, in order.
fn fix_commands(fix: &Fix) -> Vec<Vec<String>> {
    let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    match fix {
        Fix::StaleLock => vec![argv(&format!("sudo rm -f -- {DB_LOCK}"))],
        Fix::Units { user, names } => {
            let base = if *user { "systemctl --user" } else { "sudo systemctl" };
            ["reset-failed", "restart"]
                .iter()
                .map(|verb| [argv(&format!("{base} {verb}")), names.clone()].concat())
                .collect()
        }
        Fix::Orphans(names) => vec![[argv("sudo pacman -Rns --noconfirm --"), names.clone()].concat()],
        Fix::Journal => vec![argv("sudo journalctl --vacuum-size=500M")],
        Fix::PkgCache => vec![argv("sudo paccache -rk2")],
    }
}

fn fix_valid(fix: &Fix) -> bool {
    match fix {
        Fix::Units { names, .. } => names.iter().all(|n| valid_unit(n)),
        Fix::Orphans(names) => names.iter().all(|n| valid_pkg(n)),
        _ => true,
    }
}

fn confirm(question: &str) -> bool {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == "evet"
}

fn apply(fix: &Fix) -> Result {
    // The lock may have become a live one while the user was reading.
    if *fix == Fix::StaleLock && pacman_running() {
        return Err("pacman şu an çalışıyor, kilit silinmedi".into());
    }
    for argv in fix_commands(fix) {
        run(Command::new(&argv[0]).args(&argv[1..]))?;
    }
    Ok(())
}

/// Offers every fix; true when at least one was applied.
fn repair(findings: &[Finding]) -> bool {
    let mut applied = false;
    for (f, fix) in findings.iter().filter_map(|f| Some((f, f.fix.as_ref()?))) {
        if !fix_valid(fix) {
            continue;
        }
        println!("\n{}", f.text.lines().next().unwrap_or_default());
        if let Fix::Orphans(names) = fix {
            // -s also removes dependencies that become unneeded: show the real removal set.
            let mut argv = vec!["pacman", "-Rnsp", "--print-format", "%n", "--"];
            argv.extend(names.iter().map(String::as_str));
            if let Some(list) = capture(&argv) {
                println!("  Silinecek paketler: {}", list.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        for argv in fix_commands(fix) {
            println!("  $ {}", argv.join(" "));
        }
        if !confirm("Uygulansın mı? 'evet' yaz:") {
            println!("  atlandı");
            continue;
        }
        applied = true;
        if let Err(e) = apply(fix) {
            eprintln!("osmium-doctor: {e}");
        }
    }
    if !findings.iter().any(|f| f.fix.is_some()) {
        println!("\nOnarılacak bir şey yok.");
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_units() {
        let out = "nginx.service        loaded failed failed A high performance web server\n\
                   ● foo@bar.service loaded failed failed Foo\n\
                   systemd-fsck@dev-disk-by\\x2duuid-1.service loaded failed failed fsck\n";
        let units = parse_units(out);
        assert_eq!(units, ["nginx.service", "foo@bar.service", "systemd-fsck@dev-disk-by\\x2duuid-1.service"]);
        assert!(parse_units("").is_empty());
        assert!(parse_units("--force.service loaded failed\n$(id).service x\n").is_empty());

        let f = units_finding(false, units);
        assert_eq!(f.sev, Sev::Bad);
        assert!(f.text.starts_with("3 başarısız sistem birimi: nginx.service"));
        assert_eq!(units_finding(true, vec![]).sev, Sev::Ok);
    }

    #[test]
    fn disk_usage() {
        let out = "Use% Mounted on\n 45% /\n 95% /home\n";
        let v = parse_df(out);
        assert_eq!(v, [("/".to_string(), 45), ("/home".to_string(), 95)]);
        assert_eq!(disk_finding("/", 45).sev, Sev::Ok);
        assert_eq!(disk_finding("/", 90).sev, Sev::Ok);
        assert_eq!(disk_finding("/home", 95).sev, Sev::Bad);
        // /home on the root filesystem is reported as `/` twice.
        assert_eq!(parse_df("Use% Mounted on\n 45% /\n 45% /\n").len(), 1);
    }

    #[test]
    fn btrfs_stats() {
        let out = "[/dev/nvme0n1p2].write_io_errs    0\n[/dev/nvme0n1p2].read_io_errs     0\n\
                   [/dev/nvme0n1p2].flush_io_errs    0\n[/dev/nvme0n1p2].corruption_errs  7\n\
                   [/dev/nvme0n1p2].generation_errs  0\n";
        let errs = parse_btrfs_stats(out);
        assert_eq!(errs, [("[/dev/nvme0n1p2].corruption_errs".to_string(), 7)]);
        assert_eq!(btrfs_finding(&errs).sev, Sev::Bad);
        assert_eq!(btrfs_finding(&[]).sev, Sev::Ok);
    }

    #[test]
    fn stale_lock_only_without_pacman() {
        assert_eq!(lock_finding(true, false).fix, Some(Fix::StaleLock));
        assert_eq!(lock_finding(true, true).sev, Sev::Ok);
        assert_eq!(lock_finding(false, false).sev, Sev::Ok);
    }

    #[test]
    fn missing_files() {
        let out = "foo /usr/bin/foo\nfoo /usr/share/foo/a\nbar /usr/bin/bar\n";
        let f = missing_finding(out);
        assert_eq!(f.sev, Sev::Bad);
        assert!(f.text.starts_with("2 pakette 3 eksik dosya: foo, bar "), "{}", f.text);
        assert_eq!(missing_finding("").sev, Sev::Ok);
    }

    #[test]
    fn orphans() {
        let names = parse_names("libfoo\nqt5-base\n--overwrite\n$(reboot)\nlibc++\n\n");
        assert_eq!(names, ["libfoo", "qt5-base", "libc++"]);
        let f = orphans_finding(names.clone());
        assert_eq!((f.sev, &f.fix), (Sev::Warn, &Some(Fix::Orphans(names))));
        assert_eq!(orphans_finding(vec![]).sev, Sev::Ok);
    }

    #[test]
    fn journal_messages_deduplicated() {
        let out = "usb 1-2: device descriptor read/64, error -71\nusb 1-4: device descriptor read/64, error -71\n\
                   Failed to start Foo.\n\nusb 3-1: device descriptor read/64, error -71\n";
        let (total, top) = top_messages(out, 5);
        assert_eq!(total, 4);
        assert_eq!(top[0], (3, "usb #-#: device descriptor read/#, error -#".to_string()));
        assert_eq!(top[1], (1, "Failed to start Foo.".to_string()));
        assert_eq!(top_messages(out, 1).1.len(), 1);

        let f = journal_finding(out);
        assert_eq!(f.sev, Sev::Warn);
        assert!(f.text.contains("4 hata") && f.text.contains("3x usb"));
        assert_eq!(journal_finding("\n").sev, Sev::Ok);
    }

    #[test]
    fn journal_disk_usage() {
        assert_eq!(journal_usage("Archived and active journals take up 1.5G in the file system.\n"), Some(3 << 29));
        assert_eq!(journal_usage("Archived and active journals take up 856.0M in the file system."), Some(856 << 20));
        assert_eq!(journal_usage("nothing here"), None);
    }

    #[test]
    fn pacnew_files() {
        let f = pacnew_finding("/etc/pacman.conf.pacnew\n/etc/foo.pacsave\n/etc/locale.gen.pacnew\n");
        assert_eq!(f.sev, Sev::Warn);
        assert!(f.text.starts_with("2 bekleyen") && !f.text.contains("pacsave"));
        assert_eq!(pacnew_finding("").sev, Sev::Ok);
    }

    #[test]
    fn cache_dry_run() {
        assert_eq!(cache_candidates("==> finished dry run: 12 candidates (disk space saved: 1.21 GiB)\n"), 12);
        assert_eq!(cache_candidates("==> no candidate packages found for pruning\n"), 0);
        assert_eq!(cache_candidates(""), 0);
    }

    #[test]
    fn snapper() {
        assert_eq!(snapper_count("number\n0\n1\n2\n17\n"), 3);
        assert_eq!(snapper_count("number\n0\n"), 0);
        assert_eq!(snapper_finding(0, true).sev, Sev::Warn);
        assert_eq!(snapper_finding(3, false).sev, Sev::Warn);
        assert_eq!(snapper_finding(3, true).sev, Sev::Ok);
    }

    #[test]
    fn memory() {
        let text = "MemTotal:       16000000 kB\nMemFree:          200000 kB\nMemAvailable:    1000000 kB\n\
                    SwapCached:            0 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n";
        assert_eq!(meminfo(text), Some((16000000, 1000000, 0)));
        assert_eq!(meminfo("MemTotal: 1 kB\n"), None);
        assert_eq!(mem_finding(16000000, 1000000, 0).sev, Sev::Warn);
        assert_eq!(mem_finding(16000000, 1000000, 8000000).sev, Sev::Ok, "zram or swap is enough");
        assert_eq!(mem_finding(16000000, 8000000, 0).sev, Sev::Ok);
    }

    #[test]
    fn fix_command_lines() {
        let cmds = |f: &Fix| fix_commands(f).iter().map(|a| a.join(" ")).collect::<Vec<_>>();
        assert_eq!(cmds(&Fix::StaleLock), ["sudo rm -f -- /var/lib/pacman/db.lck"]);
        assert_eq!(
            cmds(&Fix::Units { user: false, names: vec!["a.service".into(), "b.timer".into()] }),
            ["sudo systemctl reset-failed a.service b.timer", "sudo systemctl restart a.service b.timer"]
        );
        assert_eq!(
            cmds(&Fix::Units { user: true, names: vec!["a.service".into()] }),
            ["systemctl --user reset-failed a.service", "systemctl --user restart a.service"]
        );
        assert_eq!(cmds(&Fix::Orphans(vec!["x".into(), "y".into()])), ["sudo pacman -Rns --noconfirm -- x y"]);
        assert_eq!(cmds(&Fix::Journal), ["sudo journalctl --vacuum-size=500M"]);
        assert_eq!(cmds(&Fix::PkgCache), ["sudo paccache -rk2"]);
        assert!(!fix_valid(&Fix::Orphans(vec!["-x".into()])));
        assert!(!fix_valid(&Fix::Units { user: false, names: vec!["--now".into()] }));
    }

    #[test]
    fn report_counts_and_labels() {
        let r = report(&[finding(Sev::Bad, "a"), finding(Sev::Warn, "b"), finding(Sev::Ok, "c")]);
        assert_eq!(r, "sorun  a\nuyarı  b\ntamam  c\n\n1 sorun, 1 uyarı\n");
    }
}
