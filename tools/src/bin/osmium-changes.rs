//! "What changed?" between two points in time, from snapper snapshots.
//!
//!   osmium-changes list             snapshots (number, date, description)
//!   osmium-changes [<from>] [<to>]  packages, /etc files, enabled units, kernel
//!   osmium-changes --ai "<soru>"    ask osmium-ai, with the report as context
//!   osmium-changes undo <from>      roll back to <from> after an `evet`
//!
//! Defaults: from = the newest snapshot older than 24 h (or the oldest one),
//! to = the live system. /.snapshots is root-only, so the read-only report and
//! the list re-run themselves through sudo.

use osmium_tools::{Result, exit_on_err, output, run};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const SNAPSHOTS: &str = "/.snapshots";
const ETC_CAP: usize = 40;
const AI_CAP: usize = 6000;
const DAY: u64 = 24 * 60 * 60;
const KERNELS: &[&str] = &["linux", "linux-lts", "linux-zen", "linux-hardened"];
const USAGE: &str = "kullanım: osmium-changes list | [<nereden>] [<nereye>] | --ai \"<soru>\" | undo <nereden>";

#[derive(Debug, PartialEq)]
enum Cmd {
    List,
    Report(Option<u32>, Option<u32>),
    Ai(String),
    Undo(u32),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    exit_on_err("osmium-changes", parse_args(&args).and_then(|cmd| dispatch(cmd, &args)));
}

fn snapshot_number(s: &str) -> Result<u32> {
    s.parse().map_err(|_| format!("`{s}` bir anlık görüntü numarası değil (bkz: osmium-changes list)"))
}

fn parse_args(args: &[String]) -> Result<Cmd> {
    Ok(match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["list"] => Cmd::List,
        ["--ai", q] if !q.trim().is_empty() => Cmd::Ai(q.into()),
        ["undo", from] => match snapshot_number(from)? {
            0 => return Err("0 canlı sistemdir, geri alınacak bir şey yok".into()),
            n => Cmd::Undo(n),
        },
        [] => Cmd::Report(None, None),
        [from] if !from.starts_with('-') && from != "undo" => Cmd::Report(Some(snapshot_number(from)?), None),
        [from, to] if !from.starts_with('-') && from != "undo" => {
            Cmd::Report(Some(snapshot_number(from)?), Some(snapshot_number(to)?))
        }
        _ => return Err(USAGE.into()),
    })
}

fn dispatch(cmd: Cmd, args: &[String]) -> Result {
    let root = is_root();
    match cmd {
        Cmd::Undo(from) => undo(from, root),
        Cmd::Ai(question) => {
            let report = if root { build_report(None, None)? } else { output(sudo_self(&[])?.stdin(Stdio::inherit()).stderr(Stdio::inherit()))? };
            let prompt = format!("{question}\n\nSistemde şu değişiklikler oldu:\n{report}");
            run(Command::new("osmium-ai").args(["ask", &prompt.chars().take(AI_CAP).collect::<String>()]))
        }
        _ if !root => {
            // The root copy already printed its own error: pass its exit code on, nothing more.
            let status = sudo_self(args)?.status().map_err(|e| format!("sudo: {e}"))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::List => list(),
        Cmd::Report(from, to) => {
            print!("{}", build_report(from, to)?);
            Ok(())
        }
    }
}

fn is_root() -> bool {
    output(Command::new("id").arg("-u")).is_ok_and(|s| s.trim() == "0")
}

/// This program again, as root. The arguments were validated by `parse_args`.
fn sudo_self(args: &[String]) -> Result<Command> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut c = Command::new("sudo");
    c.arg("--").arg(exe).args(args);
    Ok(c)
}

// ---------- snapshots ----------

fn snap_root(n: u32) -> PathBuf {
    match n {
        0 => PathBuf::from("/"),
        n => Path::new(SNAPSHOTS).join(n.to_string()).join("snapshot"),
    }
}

fn label(n: u32) -> String {
    match n {
        0 => "canlı sistem".into(),
        n => format!("#{n}"),
    }
}

/// `(number, date, description)` rows of `snapper list`, without the live
/// system (0). Columns are found by header name, so the unicode table, the
/// ascii table and --csvout all work, whatever columns the version prints.
fn parse_snapper_list(text: &str) -> Vec<(u32, String, String)> {
    let sep = ['│', '|', ','].into_iter().find(|&c| text.contains(c)).unwrap_or(',');
    let mut cols: Option<(usize, Option<usize>, Option<usize>)> = None;
    let mut rows = Vec::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.split(sep).map(str::trim).collect();
        let Some((num, date, desc)) = cols else {
            let find = |names: &[&str]| cells.iter().position(|c| names.contains(&c.to_ascii_lowercase().as_str()));
            cols = find(&["#", "number"]).map(|n| (n, find(&["date"]), find(&["description"])));
            continue;
        };
        // The number carries a marker on the current/default snapshot: `12*`, `3-`, `3+`.
        let Some(n) = cells.get(num).and_then(|c| c.trim_end_matches(|ch: char| !ch.is_ascii_digit()).parse::<u32>().ok())
        else {
            continue;
        };
        let cell = |i: Option<usize>| i.and_then(|i| cells.get(i)).copied().unwrap_or_default().to_string();
        if n > 0 {
            rows.push((n, cell(date), cell(desc)));
        }
    }
    rows
}

fn list() -> Result {
    let text = output(Command::new("snapper").args(["--iso", "-c", "root", "list"]).env("LC_ALL", "C"))?;
    let rows = parse_snapper_list(&text);
    if rows.is_empty() {
        // Unknown layout or no snapshots: the raw table is still better than nothing.
        print!("{text}");
    }
    for (n, date, desc) in rows {
        println!("{n:>5}  {date}  {desc}");
    }
    Ok(())
}

/// The newest snapshot older than a day, else the oldest. Input: `(number, age in seconds)`.
fn pick_from(snaps: &[(u32, u64)]) -> Option<u32> {
    let old = snaps.iter().filter(|s| s.1 >= DAY).min_by_key(|s| (s.1, u32::MAX - s.0));
    old.or_else(|| snaps.iter().max_by_key(|s| (s.1, u32::MAX - s.0))).map(|s| s.0)
}

/// Numbers and ages of the snapshots on disk; info.xml is written when the snapshot is taken.
fn snapshot_ages() -> Vec<(u32, u64)> {
    let Ok(dir) = fs::read_dir(SNAPSHOTS) else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| {
            let n: u32 = e.file_name().to_str()?.parse().ok()?;
            if n == 0 || !snap_root(n).is_dir() {
                return None;
            }
            let taken = fs::metadata(e.path().join("info.xml")).or_else(|_| e.metadata()).ok()?.modified().ok()?;
            Some((n, SystemTime::now().duration_since(taken).map_or(0, |d| d.as_secs())))
        })
        .collect()
}

// ---------- packages ----------

/// `"xorg-server-21.1.13-1"` into (`"xorg-server"`, `"21.1.13-1"`): version and
/// release never contain a dash, names do.
fn split_pkg(dir: &str) -> Option<(&str, &str)> {
    let (rest, rel) = dir.rsplit_once('-')?;
    let (name, ver) = rest.rsplit_once('-')?;
    (!name.is_empty() && !ver.is_empty() && !rel.is_empty()).then(|| (name, &dir[name.len() + 1..]))
}

type Pkgs = BTreeMap<String, String>;

fn read_pkgs(root: &Path) -> Result<Pkgs> {
    let dir = root.join("var/lib/pacman/local");
    let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(entries
        .flatten()
        .filter_map(|e| split_pkg(e.file_name().to_str()?).map(|(n, v)| (n.to_string(), v.to_string())))
        .collect())
}

#[derive(Debug, Default, PartialEq)]
struct Report {
    from: String,
    to: String,
    pkg_added: Vec<String>,
    pkg_removed: Vec<String>,
    /// (name, old version, new version)
    pkg_changed: Vec<(String, String, String)>,
    etc_added: Vec<String>,
    etc_removed: Vec<String>,
    etc_changed: Vec<String>,
    units_enabled: Vec<String>,
    units_disabled: Vec<String>,
}

fn diff_pkgs(old: &Pkgs, new: &Pkgs, r: &mut Report) {
    for (name, ver) in new {
        match old.get(name) {
            None => r.pkg_added.push(format!("{name} {ver}")),
            Some(o) if o != ver => r.pkg_changed.push((name.clone(), o.clone(), ver.clone())),
            Some(_) => {}
        }
    }
    r.pkg_removed = old.iter().filter(|(n, _)| !new.contains_key(*n)).map(|(n, v)| format!("{n} {v}")).collect();
}

// ---------- /etc ----------

#[derive(Debug, PartialEq)]
enum Entry {
    File(u64, Option<SystemTime>),
    Link(PathBuf),
}

/// Files that differ on every machine and boot, and the pacman keyring.
fn skipped(rel: &str) -> bool {
    matches!(rel, "machine-id" | "ld.so.cache" | "mtab" | "resolv.conf" | "pacman.d/gnupg")
}

/// Files and symlinks under `dir`, keyed by their `/`-separated path relative to the walk root.
fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, Entry>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        let Ok(meta) = e.path().symlink_metadata() else { continue };
        if skipped(&rel) {
        } else if meta.is_symlink() {
            out.insert(rel, Entry::Link(fs::read_link(e.path()).unwrap_or_default()));
        } else if meta.is_dir() {
            walk(&e.path(), &rel, out);
        } else if meta.is_file() {
            out.insert(rel, Entry::File(meta.len(), meta.modified().ok()));
        }
    }
}

/// `"systemd/system/multi-user.target.wants/sshd.service"` into `"sshd.service (multi-user.target)"`.
fn unit_link(rel: &str) -> Option<String> {
    let (dir, unit) = rel.strip_prefix("systemd/system/")?.split_once('/')?;
    let target = dir.strip_suffix(".wants")?;
    (!unit.contains('/')).then(|| format!("{unit} ({target})"))
}

fn diff_sets(old: &BTreeSet<String>, new: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    (new.difference(old).cloned().collect(), old.difference(new).cloned().collect())
}

/// Compares two /etc trees. `same_content(rel)` is asked only when size matches but mtime does not.
fn diff_etc(old: &BTreeMap<String, Entry>, new: &BTreeMap<String, Entry>, same_content: impl Fn(&str) -> bool, r: &mut Report) {
    let units = |m: &BTreeMap<String, Entry>| m.keys().filter_map(|k| unit_link(k)).collect::<BTreeSet<_>>();
    (r.units_enabled, r.units_disabled) = diff_sets(&units(old), &units(new));
    // Unit links are reported above, not again as files.
    let files = |m: &BTreeMap<String, Entry>| m.keys().filter(|k| unit_link(k).is_none()).cloned().collect::<BTreeSet<_>>();
    (r.etc_added, r.etc_removed) = diff_sets(&files(old), &files(new));
    for (rel, o) in old.iter().filter(|(k, _)| unit_link(k).is_none()) {
        let changed = match (o, new.get(rel)) {
            (_, None) => false,
            (a, Some(b)) if a == b => false,
            (Entry::File(a, _), Some(Entry::File(b, _))) if a == b => !same_content(rel),
            _ => true,
        };
        if changed {
            r.etc_changed.push(rel.clone());
        }
    }
}

// ---------- report ----------

fn render(r: &Report) -> String {
    let mut s = format!("Değişiklikler: {} → {}\n", r.from, r.to);

    s += &format!(
        "\nPaketler: {} eklendi, {} kaldırıldı, {} sürümü değişti\n",
        r.pkg_added.len(),
        r.pkg_removed.len(),
        r.pkg_changed.len()
    );
    for p in &r.pkg_added {
        s += &format!("  + {p}\n");
    }
    for p in &r.pkg_removed {
        s += &format!("  - {p}\n");
    }
    for (name, old, new) in &r.pkg_changed {
        s += &format!("  ~ {name} {old} → {new}\n");
    }

    let is_kernel = |p: &&String| p.split(' ').next().is_some_and(|n| KERNELS.contains(&n));
    let mut kernels: Vec<String> = r
        .pkg_changed
        .iter()
        .filter(|p| KERNELS.contains(&p.0.as_str()))
        .map(|(name, old, new)| format!("{name} {old} → {new} (yeniden başlatınca etkin olur)"))
        .collect();
    kernels.extend(r.pkg_added.iter().filter(is_kernel).map(|p| format!("{p} kuruldu")));
    kernels.extend(r.pkg_removed.iter().filter(is_kernel).map(|p| format!("{p} kaldırıldı")));
    if kernels.is_empty() {
        s += "\nÇekirdek: değişmedi\n";
    }
    for k in kernels {
        s += &format!("\nÇekirdek: {k}\n");
    }

    let etc: Vec<String> = [("+", &r.etc_added), ("-", &r.etc_removed), ("~", &r.etc_changed)]
        .iter()
        .flat_map(|(mark, paths)| paths.iter().map(move |p| format!("  {mark} /etc/{p}\n")))
        .collect();
    s += &format!(
        "\n/etc: {} yeni, {} silinen, {} değişen dosya\n",
        r.etc_added.len(),
        r.etc_removed.len(),
        r.etc_changed.len()
    );
    s += &etc.iter().take(ETC_CAP).cloned().collect::<String>();
    if etc.len() > ETC_CAP {
        s += &format!("  ... ve {} dosya daha\n", etc.len() - ETC_CAP);
    }

    s += &format!("\nServisler: {} etkinleştirildi, {} devre dışı bırakıldı\n", r.units_enabled.len(), r.units_disabled.len());
    for u in &r.units_enabled {
        s += &format!("  + {u}\n");
    }
    for u in &r.units_disabled {
        s += &format!("  - {u}\n");
    }
    s
}

fn compare(old_root: &Path, new_root: &Path, from: String, to: String) -> Result<Report> {
    let mut r = Report { from, to, ..Default::default() };
    diff_pkgs(&read_pkgs(old_root)?, &read_pkgs(new_root)?, &mut r);
    let (mut old, mut new) = (BTreeMap::new(), BTreeMap::new());
    walk(&old_root.join("etc"), "", &mut old);
    walk(&new_root.join("etc"), "", &mut new);
    let same = |rel: &str| {
        let read = |root: &Path| fs::read(root.join("etc").join(rel)).ok();
        read(old_root).is_some_and(|a| Some(a) == read(new_root))
    };
    diff_etc(&old, &new, same, &mut r);
    Ok(r)
}

fn build_report(from: Option<u32>, to: Option<u32>) -> Result<String> {
    let from = match from {
        Some(n) => n,
        None => pick_from(&snapshot_ages()).ok_or("hiç snapper anlık görüntüsü yok")?,
    };
    let to = to.unwrap_or(0);
    for n in [from, to] {
        if !snap_root(n).is_dir() {
            return Err(format!("{} anlık görüntüsü yok (bkz: osmium-changes list)", label(n)));
        }
    }
    Ok(render(&compare(&snap_root(from), &snap_root(to), label(from), label(to))?))
}

// ---------- undo ----------

fn undo_argv(from: u32, root: bool) -> Vec<String> {
    let snapper = ["snapper", "-c", "root", "undochange"].map(String::from);
    let sudo = if root { vec![] } else { vec!["sudo".to_string()] };
    [sudo, snapper.to_vec(), vec![format!("{from}..0")]].concat()
}

fn undo(from: u32, root: bool) -> Result {
    let argv = undo_argv(from, root);
    println!("Bu komut #{from} anlık görüntüsünden beri değişen bütün sistem dosyalarını (paketler ve /etc dahil) o günkü haline döndürür:");
    println!("  $ {}", argv.join(" "));
    print!("Çalıştırılsın mı? 'evet' yaz: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() || line.trim() != "evet" {
        println!("vazgeçildi");
        return Ok(());
    }
    run(Command::new(&argv[0]).args(&argv[1..]))?;
    // undochange also brings back /boot/osmium.efi; put that kernel on the ESP as well, as a
    // counted entry, so a kernel that no longer boots falls back to the current one.
    let mut boot = Command::new(if root { "osmium-boot" } else { "sudo" });
    if !root {
        boot.arg("osmium-boot");
    }
    if let Err(e) = run(boot.arg("kur")) {
        eprintln!("osmium-changes: uyarı: açılış görüntüsü güncellenemedi: {e}");
    }
    println!("Geri alındı. Çekirdek ya da servisler değiştiyse yeniden başlat.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn package_dir_names() {
        assert_eq!(split_pkg("linux-6.11.5.arch1-1"), Some(("linux", "6.11.5.arch1-1")));
        assert_eq!(split_pkg("xorg-server-21.1.13-1"), Some(("xorg-server", "21.1.13-1")));
        assert_eq!(split_pkg("gtk-update-icon-cache-1:4.16.3-1"), Some(("gtk-update-icon-cache", "1:4.16.3-1")));
        assert_eq!(split_pkg("lib32-libx11-1.8.10-1.1"), Some(("lib32-libx11", "1.8.10-1.1")));
        assert_eq!(split_pkg("ALPM_DB_VERSION"), None);
        assert_eq!(split_pkg("foo-1"), None);
        assert_eq!(split_pkg("-1-1"), None);
    }

    #[test]
    fn package_diff() {
        let pkgs = |v: &[(&str, &str)]| v.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect::<Pkgs>();
        let old = pkgs(&[("bash", "5.2-1"), ("linux", "6.11.1.arch1-1"), ("nano", "8.0-1")]);
        let new = pkgs(&[("bash", "5.2-1"), ("linux", "6.11.5.arch1-1"), ("vim", "9.1-1")]);
        let mut r = Report::default();
        diff_pkgs(&old, &new, &mut r);
        assert_eq!(r.pkg_added, ["vim 9.1-1"]);
        assert_eq!(r.pkg_removed, ["nano 8.0-1"]);
        assert_eq!(r.pkg_changed, [("linux".to_string(), "6.11.1.arch1-1".to_string(), "6.11.5.arch1-1".to_string())]);
    }

    #[test]
    fn snapper_tables() {
        let unicode = "  # │ Type   │ Pre # │ Date                │ User │ Cleanup  │ Description │ Userdata\n\
                       ────┼────────┼───────┼─────────────────────┼──────┼──────────┼─────────────┼─────────\n\
                       \x20 0 │ single │       │                     │ root │          │ current     │\n\
                       \x20 1 │ single │       │ 2026-11-08 10:00:01 │ root │ timeline │ timeline    │\n\
                       \x2012* │ pre    │       │ 2026-11-09 18:30:00 │ root │ number   │ pacman -Syu │ important=yes\n";
        let want = [
            (1, "2026-11-08 10:00:01".to_string(), "timeline".to_string()),
            (12, "2026-11-09 18:30:00".to_string(), "pacman -Syu".to_string()),
        ];
        assert_eq!(parse_snapper_list(unicode), want);

        let ascii = " # | Type   | Pre # | Date                | User | Cleanup  | Description | Userdata\n\
                     ---+--------+-------+---------------------+------+----------+-------------+---------\n\
                     0  | single |       |                     | root |          | current     |\n\
                     1  | single |       | 2026-11-08 10:00:01 | root | timeline | timeline    |\n\
                     12 | pre    |       | 2026-11-09 18:30:00 | root | number   | pacman -Syu |\n";
        assert_eq!(parse_snapper_list(ascii), want);

        let csv = "number,date,description\n0,,current\n1,2026-11-08 10:00:01,timeline\n12,2026-11-09 18:30:00,pacman -Syu\n";
        assert_eq!(parse_snapper_list(csv), want);

        assert!(parse_snapper_list("").is_empty());
        assert!(parse_snapper_list("No snapshots.\n").is_empty());
    }

    #[test]
    fn default_from() {
        let h = 3600;
        assert_eq!(pick_from(&[(1, 72 * h), (2, 30 * h), (3, 25 * h), (4, 2 * h)]), Some(3));
        assert_eq!(pick_from(&[(5, 3 * h), (6, 2 * h), (7, h)]), Some(5), "none older than a day: the oldest");
        assert_eq!(pick_from(&[]), None);
    }

    #[test]
    fn unit_links() {
        assert_eq!(unit_link("systemd/system/multi-user.target.wants/sshd.service").as_deref(), Some("sshd.service (multi-user.target)"));
        assert_eq!(unit_link("systemd/system/display-manager.service"), None);
        assert_eq!(unit_link("systemd/system/sshd.service.d/override.conf"), None);
        assert_eq!(unit_link("systemd/user/default.target.wants/x.service"), None);
    }

    #[test]
    fn etc_diff_on_real_trees() {
        let base = std::env::temp_dir().join(format!("osmium-changes-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (old, new) = (base.join("old"), base.join("new"));
        let put = |root: &Path, rel: &str, text: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        for root in [&old, &new] {
            put(root, "var/lib/pacman/local/bash-5.2-1/desc", "");
            put(root, "etc/same.conf", "same");
            put(root, "etc/machine-id", if root == &old { "aaaa" } else { "bbbb" });
            put(root, "etc/pacman.d/gnupg/trustdb.gpg", if root == &old { "a" } else { "bb" });
            put(root, "etc/systemd/system/multi-user.target.wants/keep.service", "");
        }
        put(&old, "etc/hosts", "127.0.0.1 a");
        put(&new, "etc/hosts", "127.0.0.1 b");
        // Both writes can land in one clock tick; an edit always moves mtime on a real system.
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::options().write(true).open(new.join("etc/hosts")).unwrap().set_modified(later).unwrap();
        put(&old, "etc/sub/grown.conf", "x");
        put(&new, "etc/sub/grown.conf", "xx");
        put(&old, "etc/gone.conf", "x");
        put(&new, "etc/new.conf", "x");
        put(&new, "var/lib/pacman/local/vim-9.1-1/desc", "");
        put(&old, "etc/systemd/system/multi-user.target.wants/old.service", "");
        put(&new, "etc/systemd/system/timers.target.wants/new.timer", "");

        let r = compare(&old, &new, "#1".into(), "canlı sistem".into()).unwrap();
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(r.etc_added, ["new.conf"]);
        assert_eq!(r.etc_removed, ["gone.conf"]);
        assert_eq!(r.etc_changed, ["hosts", "sub/grown.conf"], "same size is compared by content");
        assert_eq!(r.units_enabled, ["new.timer (timers.target)"]);
        assert_eq!(r.units_disabled, ["old.service (multi-user.target)"]);
        assert_eq!(r.pkg_added, ["vim 9.1-1"]);
    }

    #[test]
    fn report_text() {
        let r = Report {
            from: "#3".into(),
            to: "canlı sistem".into(),
            pkg_added: strings(&["vim 9.1-1"]),
            pkg_removed: strings(&["nano 8.0-1"]),
            pkg_changed: vec![("linux".into(), "6.11.1.arch1-1".into(), "6.11.5.arch1-1".into())],
            etc_added: strings(&["new.conf"]),
            etc_removed: vec![],
            etc_changed: strings(&["hosts"]),
            units_enabled: strings(&["sshd.service (multi-user.target)"]),
            units_disabled: vec![],
        };
        assert_eq!(
            render(&r),
            "Değişiklikler: #3 → canlı sistem\n\
             \nPaketler: 1 eklendi, 1 kaldırıldı, 1 sürümü değişti\n  + vim 9.1-1\n  - nano 8.0-1\n  ~ linux 6.11.1.arch1-1 → 6.11.5.arch1-1\n\
             \nÇekirdek: linux 6.11.1.arch1-1 → 6.11.5.arch1-1 (yeniden başlatınca etkin olur)\n\
             \n/etc: 1 yeni, 0 silinen, 1 değişen dosya\n  + /etc/new.conf\n  ~ /etc/hosts\n\
             \nServisler: 1 etkinleştirildi, 0 devre dışı bırakıldı\n  + sshd.service (multi-user.target)\n"
        );
    }

    #[test]
    fn etc_list_is_capped() {
        let r = Report { etc_changed: (0..45).map(|i| format!("f{i:02}")).collect(), ..Default::default() };
        let text = render(&r);
        assert_eq!(text.matches("  ~ /etc/").count(), ETC_CAP);
        assert!(text.contains("... ve 5 dosya daha") && text.contains("Çekirdek: değişmedi"));
    }

    #[test]
    fn kernel_swap_is_a_kernel_change() {
        let r = Report {
            pkg_added: strings(&["linux-lts 6.6.60-1", "linux-firmware 20241111-1"]),
            pkg_removed: strings(&["linux 6.11.5.arch1-1"]),
            ..Default::default()
        };
        let text = render(&r);
        assert!(text.contains("\nÇekirdek: linux-lts 6.6.60-1 kuruldu\n"), "{text}");
        assert!(text.contains("\nÇekirdek: linux 6.11.5.arch1-1 kaldırıldı\n"), "{text}");
        assert!(!text.contains("değişmedi") && !text.contains("Çekirdek: linux-firmware"));
    }

    #[test]
    fn arguments() {
        let parse = |v: &[&str]| parse_args(&strings(v));
        assert_eq!(parse(&[]), Ok(Cmd::Report(None, None)));
        assert_eq!(parse(&["12"]), Ok(Cmd::Report(Some(12), None)));
        assert_eq!(parse(&["12", "15"]), Ok(Cmd::Report(Some(12), Some(15))));
        assert_eq!(parse(&["list"]), Ok(Cmd::List));
        assert_eq!(parse(&["--ai", "neden yavaş?"]), Ok(Cmd::Ai("neden yavaş?".into())));
        assert_eq!(parse(&["undo", "7"]), Ok(Cmd::Undo(7)));
        assert!(parse(&["undo", "0"]).is_err());
        assert!(parse(&["undo", "7..0; reboot"]).is_err());
        assert!(parse(&["../../etc"]).is_err());
        assert_eq!(parse(&["--help"]), Err(USAGE.to_string()));
        assert_eq!(parse(&["--ai"]), Err(USAGE.to_string()));
    }

    #[test]
    fn undo_command() {
        assert_eq!(undo_argv(7, false).join(" "), "sudo snapper -c root undochange 7..0");
        assert_eq!(undo_argv(7, true).join(" "), "snapper -c root undochange 7..0");
    }
}
