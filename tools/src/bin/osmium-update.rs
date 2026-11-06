//! Update risk check: looks at Arch News and freshly opened bug reports for the
//! packages about to be upgraded, and holds a risky update for a few days.
//!
//!   osmium-update check          report only (exit 2 when the update is held)
//!   osmium-update [run] [--force] check, then `sudo pacman -Syu` unless held
//!   osmium-update notify         check and send a desktop notification (user timer)
//!
//! Arch does not support partial upgrades, so a risky update is delayed as a
//! whole, never split. A hold expires after HOLD_DAYS so security fixes are not
//! blocked forever. Snapshots before/after the upgrade come from snap-pac.

use osmium_tools::{Result, exit_on_err, output, run, state_home, succeeds};
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
    let res = match args[..] {
        ["check"] => check().map(|held| if held { std::process::exit(2) }),
        [] | ["run"] => upgrade(false),
        ["--force"] | ["run", "--force"] => upgrade(true),
        ["notify"] => notify(),
        _ => Err("usage: osmium-update [check | run [--force] | notify]".into()),
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
    let pending = pending_updates()?;
    let since = read_time("last-update")?.unwrap_or(now().saturating_sub(DEFAULT_NEWS_DAYS * DAY));
    let news = match curl(NEWS_FEED) {
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

fn upgrade(force: bool) -> Result {
    let r = gather()?;
    print_report(&r);
    if r.pending.is_empty() {
        println!("Sistem güncel.");
        return Ok(());
    }
    if decide(&r)? && !force {
        println!("\nRiskli güncelleme bekletildi. Ayrıntılar yukarıda; zorlamak için: osmium-update --force");
        return Ok(());
    }
    if r.news.iter().any(|(_, hit)| *hit) && !confirm("\n'!' işaretli haberler bekleyen paketlerden bahsediyor. Okudunuz mu, devam edilsin mi?") {
        return Ok(());
    }
    run(Command::new("sudo").args(["pacman", "-Syu"]))?;
    write_time("last-update", now())?;
    let _ = fs::remove_file(state("update-hold")?);
    Ok(())
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

    #[test]
    fn reads_total_header() {
        let h = "HTTP/1.1 200 OK\r\nX-Next-Page: \r\nX-Total: 3\r\n";
        assert_eq!(header(h, "x-total"), Some("3"));
        assert_eq!(header(h, "x-missing"), None);
    }
}
