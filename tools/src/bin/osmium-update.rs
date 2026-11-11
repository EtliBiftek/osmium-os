//! Update risk check: looks at Arch News and freshly opened bug reports for the
//! packages about to be upgraded, and holds a risky update for a few days.
//!
//!   osmium-update check          report only (exit 2 when the update is held)
//!   osmium-update [run] [--force] [--provasiz]
//!                                check, rehearse, then upgrade unless held or regressed
//!   osmium-update prova          rehearse only (exit 3 on regressions)
//!   osmium-update notify         check and send a desktop notification (user timer)
//!
//! Arch does not support partial upgrades, so a risky update is delayed as a
//! whole, never split. A hold expires after HOLD_DAYS so security fixes are not
//! blocked forever. Snapshots before/after the upgrade come from snap-pac.
//!
//! Rehearsal: the upgrade first runs in a writable btrfs snapshot of the root
//! (/.osmium-prova), which is then booted headless in systemd-nspawn. Units
//! that fail there but not on the live system block the real upgrade (exit 3)
//! unless --force. The real upgrade is `pacman -Su` with the snapshot's sync
//! databases, so it installs exactly the rehearsed versions. `--provasiz`
//! skips all of this and runs `sudo pacman -Syu` as before.

use osmium_tools::{Result, catch_interrupts, exit_on_err, interrupted, output, run, state_home, succeeds};
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const NEWS_FEED: &str = "https://archlinux.org/feeds/news/";
const ISSUES_API: &str = "https://gitlab.archlinux.org/api/v4/projects/archlinux%2Fpackaging%2Fpackages%2F";
const DAY: u64 = 86_400;
/// Bug reports opened within this window count as a signal.
const ISSUE_WINDOW_DAYS: u64 = 7;
/// This many fresh reports for one package hold the update.
const HOLD_THRESHOLD: u32 = 2;
const HOLD_DAYS: u64 = 3;
/// First run: how far back to show news.
const DEFAULT_NEWS_DAYS: u64 = 14;

#[derive(Debug, PartialEq)]
struct Pending {
    name: String,
    old: String,
    new: String,
}

#[derive(Debug, PartialEq)]
struct News {
    title: String,
    link: String,
    date: u64,
    text: String,
}

struct Report {
    pending: Vec<Pending>,
    /// News since the last update; `true` if it names a pending package.
    news: Vec<(News, bool)>,
    /// Packages with fresh bug reports: (pkgbase, count).
    issues: Vec<(String, u32)>,
}

impl Report {
    fn risky(&self) -> Vec<&(String, u32)> {
        self.issues.iter().filter(|(_, n)| *n >= HOLD_THRESHOLD).collect()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (flags, words): (Vec<&str>, Vec<&str>) = args.iter().partition(|a| a.starts_with('-'));
    let (force, rehearse) = (flags.contains(&"--force"), !flags.contains(&"--provasiz"));
    let usage = || Err("usage: osmium-update [check | run [--force] [--provasiz] | prova | notify]".into());
    let blocked = |r: Result<bool>| r.map(|b| if b { std::process::exit(3) });
    let res = match words[..] {
        _ if flags.iter().any(|f| !matches!(*f, "--force" | "--provasiz")) => usage(),
        [] | ["run"] => blocked(upgrade(force, rehearse)),
        _ if !flags.is_empty() => usage(),
        ["check"] => check().map(|held| if held { std::process::exit(2) }),
        ["prova"] => blocked(prova()),
        ["notify"] => notify(),
        _ => usage(),
    };
    exit_on_err("osmium-update", res);
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn state(name: &str) -> Result<PathBuf> {
    Ok(state_home()?.join("osmium").join(name))
}

fn read_time(name: &str) -> Result<Option<u64>> {
    Ok(fs::read_to_string(state(name)?).ok().and_then(|s| s.trim().parse().ok()))
}

fn write_time(name: &str, t: u64) -> Result {
    let p = state(name)?;
    fs::create_dir_all(p.parent().unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
    fs::write(&p, t.to_string()).map_err(|e| format!("{}: {e}", p.display()))
}

// ---------- gathering ----------

fn pending_updates() -> Result<Vec<Pending>> {
    let out = Command::new("checkupdates").output().map_err(|e| format!("checkupdates: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(parse_checkupdates(&String::from_utf8_lossy(&out.stdout))),
        Some(2) => Ok(Vec::new()), // no updates
        _ => Err(format!("checkupdates failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
    }
}

fn parse_checkupdates(text: &str) -> Vec<Pending> {
    text.lines()
        .filter_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [name, old, "->", new, ..] => Some(Pending { name: name.into(), old: old.into(), new: new.into() }),
            _ => None,
        })
        .collect()
}

fn curl(url: &str) -> Result<String> {
    output(Command::new("curl").args(["-fsSL", "--max-time", "20", url]))
}

/// Number of issues the GitLab API reports for this package base, via the X-Total header.
fn fresh_issues(pkgbase: &str, since: u64) -> Result<Option<u32>> {
    let url = format!(
        "{ISSUES_API}{}/issues?state=opened&per_page=1&created_after={}",
        pkgbase.replace('+', "%2B"),
        iso8601(since)
    );
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "20", "-o", "/dev/null", "-D", "-", &url])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    let headers = String::from_utf8_lossy(&out.stdout);
    if headers.lines().next().is_some_and(|l| l.contains(" 404")) {
        return Ok(None); // not a package base
    }
    Ok(header(&headers, "x-total").and_then(|v| v.parse().ok()))
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Split packages (linux-firmware-amd) file bugs under their base (linux-firmware).
fn pkgbase(name: &str) -> Option<String> {
    let json = curl(&format!("https://archlinux.org/packages/search/json/?name={name}")).ok()?;
    let rest = &json[json.find("\"pkgbase\"")? + 9..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

fn issues_for(pending: &[Pending]) -> Vec<(String, u32)> {
    let since = now().saturating_sub(ISSUE_WINDOW_DAYS * DAY);
    let mut found: Vec<(String, u32)> = Vec::new();
    // 8 requests at a time: fast enough for a few hundred packages, polite to the server.
    for chunk in pending.chunks(8) {
        let results: Vec<Option<(String, u32)>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|p| {
                    s.spawn(move || {
                        let base = match fresh_issues(&p.name, since).ok()? {
                            Some(n) => return Some((p.name.clone(), n)),
                            None => pkgbase(&p.name)?,
                        };
                        Some((base.clone(), fresh_issues(&base, since).ok()??))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        for (base, n) in results.into_iter().flatten() {
            if n > 0 && !found.iter().any(|(b, _)| *b == base) {
                found.push((base, n));
            }
        }
    }
    found.sort_by_key(|a| std::cmp::Reverse(a.1));
    found
}

fn gather() -> Result<Report> {
    // checkupdates syncs a copy of the databases; the news feed downloads meanwhile.
    let (pending, feed) = std::thread::scope(|s| {
        let feed = s.spawn(|| curl(NEWS_FEED));
        (pending_updates(), feed.join().unwrap_or_else(|_| Err("thread panicked".into())))
    });
    let pending = pending?;
    let since = read_time("last-update")?.unwrap_or(now().saturating_sub(DEFAULT_NEWS_DAYS * DAY));
    let news = match feed {
        Ok(xml) => parse_news(&xml)
            .into_iter()
            .filter(|n| n.date > since)
            .map(|n| {
                let hit = pending.iter().any(|p| mentions(&n.title, &p.name) || mentions(&n.text, &p.name));
                (n, hit)
            })
            .collect(),
        Err(e) => {
            eprintln!("osmium-update: warning: could not read Arch News: {e}");
            Vec::new()
        }
    };
    let issues = if pending.is_empty() { Vec::new() } else { issues_for(&pending) };
    Ok(Report { pending, news, issues })
}

// ---------- parsing ----------

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(&xml[start..end])
}

fn parse_news(xml: &str) -> Vec<News> {
    xml.split("<item>")
        .skip(1)
        .filter_map(|item| {
            Some(News {
                title: unescape(tag(item, "title")?),
                link: tag(item, "link")?.to_string(),
                date: parse_rfc2822(tag(item, "pubDate")?)?,
                text: unescape(tag(item, "description").unwrap_or("")),
            })
        })
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Whole-word, case-insensitive match; package-name characters count as word characters.
fn mentions(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || "-_+.@".contains(c);
    let (text, name) = (text.to_ascii_lowercase(), name.to_ascii_lowercase());
    text.match_indices(&name).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + name.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(|c| is_word(c) && c != '.')
    })
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// "Wed, 21 Oct 2026 19:29:36 +0000" -> unix seconds.
fn parse_rfc2822(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, mon, year, time, zone] = parts[..] else { return None };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = MONTHS.iter().position(|x| *x == mon)? as u32 + 1;
    let hms: Vec<i64> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, mi, sec] = hms[..] else { return None };
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let off = zone.get(1..)?.parse::<i64>().ok()?;
    let off = sign * ((off / 100) * 3600 + (off % 100) * 60);
    let t = days_from_civil(year.parse().ok()?, m, day.parse().ok()?) * DAY as i64 + h * 3600 + mi * 60 + sec - off;
    u64::try_from(t).ok()
}

fn iso8601(t: u64) -> String {
    let (y, m, d) = civil_from_days((t / DAY) as i64);
    let s = t % DAY;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

// ---------- commands ----------

fn print_report(r: &Report) {
    println!("{} güncelleme bekliyor.", r.pending.len());
    if !r.news.is_empty() {
        println!("\nArch Haberleri (son güncellemeden beri):");
        for (n, hit) in &r.news {
            let mark = if *hit { "!" } else { " " };
            println!(" {mark} {}\n     {}", n.title, n.link);
        }
    }
    if !r.issues.is_empty() {
        println!("\nSon {ISSUE_WINDOW_DAYS} günde açılan hata kayıtları:");
        for (base, n) in &r.issues {
            let mark = if *n >= HOLD_THRESHOLD { "!" } else { " " };
            println!(" {mark} {base}: {n}  https://gitlab.archlinux.org/archlinux/packaging/packages/{base}/-/issues");
        }
    }
}

/// Returns whether the update is held. Starts or keeps the hold clock.
fn decide(r: &Report) -> Result<bool> {
    if r.risky().is_empty() {
        let _ = fs::remove_file(state("update-hold")?);
        return Ok(false);
    }
    let since = match read_time("update-hold")? {
        Some(t) => t,
        None => {
            write_time("update-hold", now())?;
            now()
        }
    };
    Ok(now().saturating_sub(since) < HOLD_DAYS * DAY)
}

fn check() -> Result<bool> {
    let r = gather()?;
    print_report(&r);
    let held = decide(&r)?;
    if held {
        let names: Vec<&str> = r.risky().iter().map(|(b, _)| b.as_str()).collect();
        println!(
            "\nGüncelleme bekletiliyor: {} için yeni hata kayıtları var. En fazla {HOLD_DAYS} gün beklenir; \
             hemen güncellemek için: osmium-update --force",
            names.join(", ")
        );
    } else if !r.risky().is_empty() {
        println!("\n{HOLD_DAYS} günlük bekleme doldu; dikkatli güncelleyin.");
    }
    Ok(held)
}

fn confirm(question: &str) -> bool {
    print!("{question} [e/H] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "e" | "E" | "evet" | "y")
}

/// Returns whether the rehearsal blocked the upgrade.
fn upgrade(force: bool, rehearse: bool) -> Result<bool> {
    let r = gather()?;
    print_report(&r);
    if r.pending.is_empty() {
        println!("Sistem güncel.");
        return Ok(false);
    }
    if decide(&r)? && !force {
        println!("\nRiskli güncelleme bekletildi. Ayrıntılar yukarıda; zorlamak için: osmium-update --force");
        return Ok(false);
    }
    if r.news.iter().any(|(_, hit)| *hit) && !confirm("\n'!' işaretli haberler bekleyen paketlerden bahsediyor. Okudunuz mu, devam edilsin mi?") {
        return Ok(false);
    }
    if rehearse {
        let findings = rehearsal(kernel_upgraded(&r.pending), &|f| f.is_empty() || force)?;
        println!("\n{}", verdict(&findings, force));
        if !findings.is_empty() && !force {
            return Ok(true);
        }
        // -Su, not -Syu: the sync databases are the rehearsed ones.
        let before = installed();
        if let Err(e) = run(Command::new("sudo").args(["pacman", "-Su", "--noconfirm"])) {
            return Err(after_failed_upgrade(e, before.is_some() && installed() == before));
        }
        let _ = run(&mut cmd(&sudo(&["rm", "-rf", SYNC_BACKUP])));
    } else {
        run(Command::new("sudo").args(["pacman", "-Syu"]))?;
    }
    write_time("last-update", now())?;
    let _ = fs::remove_file(state("update-hold")?);
    Ok(false)
}

/// `pacman -Q`: what is installed, to tell whether a failed transaction changed anything.
fn installed() -> Option<String> {
    output(Command::new("pacman").arg("-Q")).ok()
}

/// After a failed `pacman -Su` with the rehearsed sync databases. If nothing was installed,
/// the old databases come back: new databases over old packages are a partial upgrade
/// waiting to happen (any `pacman -S` would pull libraries the rest does not match).
fn after_failed_upgrade(err: String, unchanged: bool) -> String {
    if unchanged {
        let back = format!("{SYNC_BACKUP}/.");
        let restored = run(&mut cmd(&sudo(&["cp", "-a", &back, "/var/lib/pacman/sync/"])))
            .and_then(|()| run(&mut cmd(&sudo(&["rm", "-rf", SYNC_BACKUP]))));
        return match restored {
            Ok(()) => format!("güncelleme başarısız oldu, hiçbir paket değişmedi; eski paket veritabanları geri yüklendi.\n{err}"),
            Err(e) => format!("güncelleme başarısız oldu ve eski paket veritabanları geri yüklenemedi ({e}).\nYedek: {SYNC_BACKUP}\n{err}"),
        };
    }
    format!(
        "güncelleme yarıda kaldı: bazı paketler güncellendi.\n\
         Tamamlamak için: sudo pacman -Su\n\
         Geri dönmek için: osmium-changes list, sonra osmium-changes undo <güncelleme öncesi numara>.\n{err}"
    )
}

fn prova() -> Result<bool> {
    let pending = pending_updates()?;
    if pending.is_empty() {
        println!("Sistem güncel, prova edilecek güncelleme yok.");
        return Ok(false);
    }
    let findings = rehearsal(kernel_upgraded(&pending), &|_| false)?;
    println!("\n{}", verdict(&findings, false));
    Ok(!findings.is_empty())
}

// ---------- rehearsal ----------

const PROVA_ROOT: &str = "/.osmium-prova";
const PROVA_UNIT: &str = "osmium-prova.service";
/// The live sync databases while the rehearsed ones are in place, until `pacman -Su` succeeds.
const SYNC_BACKUP: &str = "/var/lib/pacman/sync.osmium-prova";
const BOOT_TIMEOUT_SECS: u64 = 180;
/// Units that cannot work in a container; their failure there says nothing.
const CONTAINER_ONLY_FAILURES: &[&str] = &[
    "systemd-modules-load",
    "sys-kernel-",
    "dev-hugepages.mount",
    "systemd-udev",
    "NetworkManager-wait-online",
    "greetd",
    // The copy holds no /.snapshots subvolume and grub-probe finds no real disk.
    "grub-btrfsd",
    "snapper-",
];

fn ignored(unit: &str) -> bool {
    CONTAINER_ONLY_FAILURES.iter().any(|p| unit.starts_with(p)) || unit.ends_with(".device") || unit.ends_with(".swap")
}

/// Unit names from `systemctl --failed --no-legend --plain`.
fn parse_failed(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().find(|w| w.contains('.')))
        .map(str::to_string)
        .collect()
}

/// Units that fail in the rehearsal but not on the live system.
fn regressions(rehearsed: &[String], live: &[String]) -> Vec<String> {
    let mut r: Vec<String> = rehearsed.iter().filter(|u| !ignored(u) && !live.contains(u)).cloned().collect();
    r.sort();
    r.dedup();
    r
}

/// `is-system-running` reply: `Some(booted)` once the boot has settled.
fn boot_settled(state: &str) -> Option<bool> {
    match state.trim() {
        "running" | "degraded" => Some(true),
        "maintenance" | "stopping" => Some(false),
        _ => None, // initializing, starting, or not reachable yet
    }
}

fn kernel_upgraded(pending: &[Pending]) -> bool {
    pending.iter().any(|p| matches!(p.name.as_str(), "linux" | "linux-lts" | "linux-zen" | "linux-hardened"))
}

/// `find -printf '%s %f\n'` of /boot: a kernel and an initramfs, none empty.
fn kernel_files_ok(listing: &str) -> bool {
    let files: Vec<(&str, &str)> = listing.lines().filter_map(|l| l.split_once(' ')).collect();
    ["vmlinuz-", "initramfs-"].iter().all(|prefix| {
        let mut sizes = files.iter().filter(|(_, name)| name.starts_with(prefix)).map(|(size, _)| *size).peekable();
        sizes.peek().is_some() && sizes.all(|s| s != "0")
    })
}

fn findings(booted: bool, regressions: &[String], deps_ok: bool, kernel_ok: bool) -> Vec<String> {
    let mut f = Vec::new();
    if !booted {
        f.push("güncellenmiş sistem provada açılmadı".to_string());
    }
    f.extend(regressions.iter().map(|u| format!("{u} yalnızca güncellemeden sonra başarısız oluyor")));
    if !deps_ok {
        f.push("paket bağımlılıkları tutarsız (pacman -Dk)".to_string());
    }
    if !kernel_ok {
        f.push("çekirdek güncellendi ama /boot içinde vmlinuz veya initramfs eksik ya da boş".to_string());
    }
    f
}

fn verdict(findings: &[String], force: bool) -> String {
    const LIMITS: &str = "Not: bu bir prova kapısıdır; donanım sürücüleri ve çekirdeğin kendisi kapsayıcıda denenmez.";
    if findings.is_empty() {
        return format!("Prova temiz: güncellenmiş sistem açıldı, yeni başarısız servis yok.\n{LIMITS}");
    }
    let list: String = findings.iter().map(|f| format!("  ! {f}\n")).collect();
    let end = if force {
        "--force verildi, yine de güncelleniyor."
    } else {
        "Güncelleme YAPILMADI. Yine de güncellemek için: osmium-update --force"
    };
    format!("Prova sorun buldu:\n{list}{LIMITS}\n{end}")
}

fn sudo(args: &[&str]) -> Vec<String> {
    std::iter::once("sudo").chain(args.iter().copied()).map(str::to_string).collect()
}

/// A one-off command in the snapshot (not booted). --as-pid2: pacman and its
/// hooks are not written to be PID 1 (zombie reaping, signals).
fn nspawn_argv(args: &[&str]) -> Vec<String> {
    let mut v = sudo(&["systemd-nspawn", "-q", "--register=no", "--as-pid2", "-D", PROVA_ROOT]);
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

fn upgrade_argv() -> Vec<String> {
    // The shared cache means packages are downloaded once. snap-pac would try to
    // snapshot from inside the container and abort the transaction.
    nspawn_argv(&["--bind=/var/cache/pacman/pkg", "--setenv=SNAP_PAC_SKIP=y", "pacman", "-Syu", "--noconfirm"])
}

/// Boots the snapshot as a transient unit; registered so `systemctl -M` works.
fn boot_argv() -> Vec<String> {
    sudo(&[
        "systemd-run", "--quiet", "--collect", "--unit", PROVA_UNIT, "-p", "KillMode=mixed", "-p", "Delegate=yes",
        "systemd-nspawn", "-q", "--register=yes", "--keep-unit", "--machine=osmium-prova", "-D", PROVA_ROOT,
        "--boot", "--private-network", "--console=passive", "--link-journal=no",
    ])
}

fn machine_argv(args: &[&str]) -> Vec<String> {
    let mut v = sudo(&["systemctl", "-M", "osmium-prova"]);
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

fn cmd(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// Stdout whatever the exit status: `is-system-running` and `--failed` exit non-zero on bad news.
fn stdout_of(argv: &[String]) -> String {
    cmd(argv)
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Stops the rehearsal container and deletes its snapshot; both may be absent.
fn discard() -> Result {
    succeeds(&mut cmd(&sudo(&["systemctl", "stop", PROVA_UNIT])));
    if succeeds(&mut cmd(&sudo(&["btrfs", "subvolume", "show", PROVA_ROOT]))) {
        run(&mut cmd(&sudo(&["btrfs", "subvolume", "delete", PROVA_ROOT])))?;
    }
    Ok(())
}

/// Runs the rehearsal and returns what it found. When `adopt` accepts the
/// findings, the snapshot's sync databases replace the live ones.
fn rehearsal(kernel: bool, adopt: &dyn Fn(&[String]) -> bool) -> Result<Vec<String>> {
    println!("\nProva: güncelleme önce sistemin bir kopyasında denenecek (yönetici parolası istenebilir).");
    // Ctrl-C now stops the commands of the rehearsal but not this process, which then
    // removes the container and the snapshot instead of leaving them for the next run.
    catch_interrupts();
    discard()?; // left over from a run that was killed
    run(&mut cmd(&sudo(&["btrfs", "subvolume", "snapshot", "/", PROVA_ROOT])))
        .map_err(|e| format!("prova için anlık görüntü alınamadı ({e}). Provasız güncelleme: osmium-update --provasiz"))?;
    let res = rehearse(kernel).and_then(|found| {
        if interrupted() {
            return Err("prova yarıda kesildi, güncelleme yapılmadı".into());
        }
        if adopt(&found) {
            let sync = format!("{PROVA_ROOT}/var/lib/pacman/sync/.");
            run(&mut cmd(&sudo(&["rm", "-rf", SYNC_BACKUP])))?;
            run(&mut cmd(&sudo(&["cp", "-a", "/var/lib/pacman/sync", SYNC_BACKUP])))?;
            run(&mut cmd(&sudo(&["cp", "-a", &sync, "/var/lib/pacman/sync/"])))?;
        }
        Ok(found)
    });
    if let Err(e) = discard() {
        eprintln!("osmium-update: uyarı: prova kopyası silinemedi: {e}");
    }
    match res {
        Err(_) if interrupted() => Err("prova yarıda kesildi, kopya silindi; güncelleme yapılmadı".into()),
        res => res,
    }
}

fn rehearse(kernel: bool) -> Result<Vec<String>> {
    run(&mut cmd(&upgrade_argv())).map_err(|e| {
        format!("güncelleme provada başarısız oldu: {e}\nProvasız güncelleme: osmium-update --provasiz")
    })?;
    let deps_ok = succeeds(&mut cmd(&nspawn_argv(&["pacman", "-Dk"])));
    let boot = format!("{PROVA_ROOT}/boot");
    let kernel_ok = !kernel
        || kernel_files_ok(&stdout_of(&sudo(&["find", &boot, "-maxdepth", "1", "-type", "f", "-printf", "%s %f\\n"])));
    // The copy must not wait for the real disks; only this throwaway fstab is emptied.
    run(&mut cmd(&sudo(&["truncate", "-s", "0", &format!("{PROVA_ROOT}/etc/fstab")])))?;

    println!("Prova: güncellenmiş sistem kapsayıcıda açılıyor...");
    run(&mut cmd(&boot_argv()))?;
    let mut booted = false;
    for _ in 0..BOOT_TIMEOUT_SECS / 2 {
        if interrupted() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        if let Some(ok) = boot_settled(&stdout_of(&machine_argv(&["is-system-running"]))) {
            booted = ok;
            break;
        }
    }
    let rehearsed = parse_failed(&stdout_of(&machine_argv(&["--failed", "--no-legend", "--plain"])));
    let live = parse_failed(&stdout_of(&["systemctl", "--failed", "--no-legend", "--plain"].map(str::to_string)));
    Ok(findings(booted, &regressions(&rehearsed, &live), deps_ok, kernel_ok))
}

fn notify() -> Result {
    let r = gather()?;
    if r.pending.is_empty() {
        return Ok(());
    }
    let held = decide(&r)?;
    let mut body = format!("{} güncelleme var.", r.pending.len());
    if held {
        let names: Vec<&str> = r.risky().iter().map(|(b, _)| b.as_str()).collect();
        body += &format!(" Bekletiliyor: {} için yeni hata kayıtları.", names.join(", "));
    }
    if r.news.iter().any(|(_, hit)| *hit) {
        body += " Güncellemeden önce okunması gereken Arch haberi var.";
    }
    body += " Ayrıntı: osmium-update check";
    let urgency = if held { "normal" } else { "low" };
    if !succeeds(Command::new("notify-send").args(["-a", "OsmiumOS", "-u", urgency, "Güncellemeler", &body])) {
        println!("{body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checkupdates() {
        let p = parse_checkupdates("mesa 1:26.2.5-1 -> 1:26.2.6-1\nlinux 7.2.13.arch1-1 -> 7.2.14.arch1-1\njunk\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], Pending { name: "mesa".into(), old: "1:26.2.5-1".into(), new: "1:26.2.6-1".into() });
    }

    #[test]
    fn date_roundtrip() {
        assert_eq!(parse_rfc2822("Thu, 01 Jan 1970 00:00:00 +0000"), Some(0));
        let t = parse_rfc2822("Wed, 21 Oct 2026 19:29:36 +0000").unwrap();
        assert_eq!(iso8601(t), "2026-10-21T19:29:36Z");
        // Offsets move to UTC.
        assert_eq!(parse_rfc2822("Wed, 21 Oct 2026 21:29:36 +0200"), Some(t));
        assert_eq!(iso8601(days_from_civil(2024, 2, 29) as u64 * DAY), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn parses_news_items() {
        let xml = "<rss><channel><title>x</title><item><title>php &amp; friends</title>\
                   <link>https://a/1</link><description>&lt;p&gt;Upgrade php now&lt;/p&gt;</description>\
                   <pubDate>Wed, 21 Oct 2026 19:29:36 +0000</pubDate></item><item><title>broken</title></item></channel></rss>";
        let n = parse_news(xml);
        assert_eq!(n.len(), 1, "items without link/date are skipped");
        assert_eq!(n[0].title, "php & friends");
        assert!(n[0].text.contains("<p>Upgrade php now"));
    }

    #[test]
    fn mentions_whole_names_only() {
        assert!(mentions("Update to php ≥ 8.5.12", "php"));
        assert!(mentions("the php.", "php"));
        assert!(!mentions("php-legacy only", "php"));
        assert!(!mentions("phpmyadmin", "php"));
        assert!(mentions("PHP-legacy ≥ 8.3", "php-legacy"));
        assert!(!mentions("linux-firmware split", "linux"));
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ignores_units_that_cannot_work_in_a_container() {
        for u in [
            "systemd-modules-load.service",
            "sys-kernel-debug.mount",
            "dev-hugepages.mount",
            "systemd-udevd.service",
            "systemd-udev-trigger.service",
            "NetworkManager-wait-online.service",
            "greetd.service",
            "dev-sda1.device",
            "dev-zram0.swap",
            "grub-btrfsd.service",
            "snapper-timeline.service",
            "snapper-cleanup.service",
        ] {
            assert!(ignored(u), "{u}");
        }
        assert!(!ignored("NetworkManager.service") && !ignored("bluetooth.service") && !ignored("home.mount"));
    }

    #[test]
    fn regressions_are_new_failures_only() {
        let out = "bluetooth.service loaded failed failed Bluetooth\n● cups.service loaded failed failed CUPS\n\
                   greetd.service loaded failed failed Greeter\nold.service loaded failed failed Old\n\n";
        let rehearsed = parse_failed(out);
        assert_eq!(rehearsed, ["bluetooth.service", "cups.service", "greetd.service", "old.service"]);
        assert_eq!(regressions(&rehearsed, &strings(&["old.service"])), ["bluetooth.service", "cups.service"]);
        assert!(regressions(&strings(&["old.service"]), &strings(&["old.service", "x.service"])).is_empty());
        assert!(parse_failed("").is_empty());
    }

    #[test]
    fn boot_states() {
        assert_eq!(boot_settled("running\n"), Some(true));
        assert_eq!(boot_settled("degraded"), Some(true));
        assert_eq!(boot_settled("maintenance"), Some(false));
        for waiting in ["", "starting", "initializing", "offline", "unknown"] {
            assert_eq!(boot_settled(waiting), None, "{waiting}");
        }
    }

    #[test]
    fn kernel_checks() {
        let p = |name: &str| Pending { name: name.into(), old: "1".into(), new: "2".into() };
        assert!(kernel_upgraded(&[p("mesa"), p("linux-zen")]));
        assert!(!kernel_upgraded(&[p("linux-firmware"), p("linux-headers")]));
        assert!(kernel_files_ok("14000000 vmlinuz-linux\n90000000 initramfs-linux.img\n0 other\n"));
        assert!(!kernel_files_ok("14000000 vmlinuz-linux\n0 initramfs-linux.img\n"));
        assert!(!kernel_files_ok("14000000 vmlinuz-linux\n"));
        assert!(!kernel_files_ok(""));
    }

    #[test]
    fn failed_upgrade_that_changed_packages_says_how_to_go_on() {
        let msg = after_failed_upgrade("pacman failed".into(), false);
        assert!(msg.contains("sudo pacman -Su") && msg.contains("osmium-changes undo") && msg.ends_with("pacman failed"), "{msg}");
    }

    #[test]
    fn verdict_texts() {
        assert!(findings(true, &[], true, true).is_empty());
        let clean = verdict(&[], false);
        assert!(clean.contains("Prova temiz") && clean.contains("donanım sürücüleri"));
        let f = findings(false, &strings(&["cups.service"]), false, false);
        assert_eq!(f.len(), 4);
        let bad = verdict(&f, false);
        assert!(bad.contains("! cups.service") && bad.contains("pacman -Dk") && bad.contains("YAPILMADI"), "{bad}");
        assert!(bad.contains("donanım sürücüleri"));
        let forced = verdict(&f, true);
        assert!(forced.contains("yine de güncelleniyor") && !forced.contains("YAPILMADI"));
    }

    #[test]
    fn rehearsal_commands() {
        assert_eq!(
            upgrade_argv().join(" "),
            "sudo systemd-nspawn -q --register=no --as-pid2 -D /.osmium-prova --bind=/var/cache/pacman/pkg \
             --setenv=SNAP_PAC_SKIP=y pacman -Syu --noconfirm"
        );
        assert_eq!(
            nspawn_argv(&["pacman", "-Dk"]).join(" "),
            "sudo systemd-nspawn -q --register=no --as-pid2 -D /.osmium-prova pacman -Dk"
        );
        let boot = boot_argv().join(" ");
        assert!(boot.starts_with("sudo systemd-run --quiet --collect --unit osmium-prova.service "), "{boot}");
        for part in ["--register=yes", "--machine=osmium-prova", "-D /.osmium-prova", "--boot", "--private-network", "--console=passive"] {
            assert!(boot.contains(part), "{part}");
        }
        assert_eq!(machine_argv(&["is-system-running"]).join(" "), "sudo systemctl -M osmium-prova is-system-running");
    }

    #[test]
    fn reads_total_header() {
        let h = "HTTP/1.1 200 OK\r\nX-Next-Page: \r\nX-Total: 3\r\n";
        assert_eq!(header(h, "x-total"), Some("3"));
        assert_eq!(header(h, "x-missing"), None);
    }
}
