//! Switches modes automatically from context: running apps, monitors, Wi-Fi,
//! battery, time of day.
//!
//!   osmium-auto run     daemon loop (user unit osmium-auto.service)
//!   osmium-auto check   show which rule matches now and why
//!   osmium-auto init    write a commented example rules file
//!
//! Rules live in ~/.config/osmium/auto.rules, one per line, first match wins:
//!
//!   <mode>: <condition> [ve <condition>]...
//!
//! When no rule matches any more, the mode that was active before osmium-auto
//! took over comes back. A manual `osmium-mode set` always wins: osmium-auto
//! stays out until the set of matching rules changes.

use osmium_tools::{Result, config_home, exit_on_err, hyprctl, json_objects, output, run, state_home};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const USAGE: &str = "kullanım: osmium-auto run | check | init";
const POWER_SUPPLY: &str = "/sys/class/power_supply";

const EXAMPLE: &str = "\
# osmium-auto kuralları: bağlama göre otomatik mod değiştirme.
#
# Biçim:  <mod>: <koşul> [ve <koşul>]...
# İlk eşleşen kural kazanır. Hiçbiri eşleşmezse önceki moda dönülür.
# Modu elle değiştirirsen, eşleşen kurallar değişene kadar karışılmaz.
#
# Koşullar:
#   uygulama=<süreç adı>   bu süreç çalışıyor
#   ekran=<sayı>           en az bu kadar ekran bağlı
#   ekran=<ad>             adı ya da tanımı bunu içeren bir ekran bağlı
#   wifi=<ağ adı>          bağlı olunan Wi-Fi ağı
#   pil<N                  pil yüzde N'in altında ve deşarj oluyor
#   saat=SS:DD-SS:DD       saat aralığı (gece yarısını aşabilir)
#   priz / pilde           prizde / pilde
#
# Modlar: osmium-mode list      Deneme: osmium-auto check

oyun: uygulama=steam
sunum: ekran=2
# calisma: wifi=OfisAgi ve saat=09:00-18:00
";

#[derive(Debug, Clone, PartialEq)]
enum Cond {
    App(String),
    Screens(usize),
    Screen(String),
    Wifi(String),
    BatteryBelow(u8),
    /// Minutes since midnight: from (inclusive), to (exclusive).
    Time(u16, u16),
    Ac,
    OnBattery,
}

#[derive(Debug, PartialEq)]
struct Rule {
    line: usize,
    mode: String,
    conds: Vec<Cond>,
}

impl Rule {
    /// Identity of a rule across edits of the file: its normalised text.
    fn key(&self) -> String {
        let conds: Vec<String> = self.conds.iter().map(ToString::to_string).collect();
        format!("{}: {}", self.mode, conds.join(" ve "))
    }
}

/// Everything the conditions look at, so they can be tested without a system.
#[derive(Debug, Default)]
struct Facts {
    apps: BTreeSet<String>,
    /// `<name> <description>` per connected monitor.
    monitors: Vec<String>,
    wifi: Option<String>,
    battery: Option<u8>,
    discharging: bool,
    minutes: Option<u16>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["run"] => daemon(),
        ["check"] => check(),
        ["init"] => init(),
        _ => Err(USAGE.into()),
    };
    exit_on_err("osmium-auto", res);
}

fn parse_rules(text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |msg: String| format!("auto.rules satır {}: {msg}", n + 1);
        let (mode, conds) = line.split_once(':').ok_or_else(|| err("`<mod>: <koşul>` bekleniyordu".into()))?;
        let mode = mode.trim();
        // The id becomes an argument of osmium-mode: keep it from looking like an option or a path.
        let valid = mode.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if mode.is_empty() || mode.starts_with('-') || !valid {
            return Err(err(format!("geçersiz mod adı `{mode}`")));
        }
        // shortcut: an SSID containing " ve " cannot be written, add quoting if someone needs it.
        let conds = conds.split(" ve ").map(|c| parse_cond(c.trim()).map_err(&err)).collect::<Result<Vec<_>>>()?;
        rules.push(Rule { line: n + 1, mode: mode.into(), conds });
    }
    Ok(rules)
}

fn parse_cond(s: &str) -> Result<Cond> {
    match s {
        "priz" => return Ok(Cond::Ac),
        "pilde" => return Ok(Cond::OnBattery),
        _ => {}
    }
    if let Some(n) = s.strip_prefix("pil<") {
        let n = n.trim().parse::<u8>().ok().filter(|n| *n <= 100);
        return n.map(Cond::BatteryBelow).ok_or_else(|| format!("`{s}`: pil<N için 0-100 arası sayı gerekli"));
    }
    let unknown = || format!("bilinmeyen koşul `{s}`");
    let (key, value) = s.split_once('=').ok_or_else(unknown)?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("`{s}`: değer boş"));
    }
    match key.trim() {
        "uygulama" => Ok(Cond::App(value.into())),
        "ekran" => Ok(value.parse().map(Cond::Screens).unwrap_or_else(|_| Cond::Screen(value.into()))),
        "wifi" => Ok(Cond::Wifi(value.into())),
        "saat" => value
            .split_once('-')
            .and_then(|(a, b)| Some(Cond::Time(hhmm(a)?, hhmm(b)?)))
            .ok_or_else(|| format!("`{s}`: saat=SS:DD-SS:DD bekleniyordu")),
        _ => Err(unknown()),
    }
}

/// `"09:30"` into minutes since midnight.
fn hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m) = (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// A process name as /proc/<pid>/comm shows it. The kernel cuts comm at 15
/// bytes, even inside a multi-byte character, so both sides go through this.
fn comm(name: &[u8]) -> String {
    let name = name.strip_suffix(b"\n").unwrap_or(name);
    String::from_utf8_lossy(&name[..name.len().min(15)]).into_owned()
}

fn in_range(from: u16, to: u16, now: u16) -> bool {
    if from <= to { from <= now && now < to } else { now >= from || now < to }
}

impl Cond {
    fn holds(&self, f: &Facts) -> bool {
        match self {
            Cond::App(name) => f.apps.contains(&comm(name.as_bytes())),
            Cond::Screens(n) => f.monitors.len() >= *n,
            Cond::Screen(text) => {
                let text = text.to_lowercase();
                f.monitors.iter().any(|m| m.to_lowercase().contains(&text))
            }
            Cond::Wifi(ssid) => f.wifi.as_deref() == Some(ssid),
            Cond::BatteryBelow(n) => f.discharging && f.battery.is_some_and(|p| p < *n),
            Cond::Time(from, to) => f.minutes.is_some_and(|now| in_range(*from, *to, now)),
            Cond::Ac => !f.discharging,
            Cond::OnBattery => f.discharging,
        }
    }
}

impl fmt::Display for Cond {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Cond::App(name) => write!(f, "uygulama={name}"),
            Cond::Screens(n) => write!(f, "ekran={n}"),
            Cond::Screen(text) => write!(f, "ekran={text}"),
            Cond::Wifi(ssid) => write!(f, "wifi={ssid}"),
            Cond::BatteryBelow(n) => write!(f, "pil<{n}"),
            Cond::Time(a, b) => write!(f, "saat={:02}:{:02}-{:02}:{:02}", a / 60, a % 60, b / 60, b % 60),
            Cond::Ac => write!(f, "priz"),
            Cond::OnBattery => write!(f, "pilde"),
        }
    }
}

fn matching<'a>(rules: &'a [Rule], facts: &Facts) -> Vec<&'a Rule> {
    rules.iter().filter(|r| r.conds.iter().all(|c| c.holds(facts))).collect()
}

/// Kept in the state dir, so a restarted daemon still knows which mode to return to.
#[derive(Debug, Default, PartialEq)]
struct State {
    /// Mode we switched to and still own.
    applied: Option<String>,
    /// Mode to return to once no rule matches.
    previous: Option<String>,
    /// Rules that matched on the last tick, by `Rule::key`: line numbers would shift
    /// when the file is edited during a manual override and read as a new match.
    matching: Vec<String>,
    /// The user switched by hand: stay out until `matching` changes.
    yielded: bool,
}

impl State {
    /// Lenient: a damaged file only loses what it cannot say.
    fn parse(text: &str) -> State {
        let mut st = State::default();
        let mode = |v: &str| Some(v.to_string()).filter(|v| !v.is_empty());
        for line in text.lines() {
            match line.split_once('=') {
                Some(("applied", v)) => st.applied = mode(v),
                Some(("previous", v)) => st.previous = mode(v),
                Some(("match", v)) => st.matching.push(v.into()),
                Some(("yielded", v)) => st.yielded = v == "1",
                _ => {}
            }
        }
        st
    }

    fn to_text(&self) -> String {
        let mut out = String::new();
        out.extend(self.applied.iter().map(|m| format!("applied={m}\n")));
        out.extend(self.previous.iter().map(|m| format!("previous={m}\n")));
        out.extend(self.matching.iter().map(|k| format!("match={k}\n")));
        if self.yielded {
            out += "yielded=1\n";
        }
        out
    }
}

fn state_file() -> Result<PathBuf> {
    Ok(state_home()?.join("osmium/auto-state"))
}

fn load_state() -> Result<State> {
    Ok(State::parse(&fs::read_to_string(state_file()?).unwrap_or_default()))
}

fn store_state(st: &State) -> Result {
    let path = state_file()?;
    let tmp = path.with_extension("tmp");
    fs::create_dir_all(path.parent().unwrap_or(&path))
        .and_then(|()| fs::write(&tmp, st.to_text()))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// One tick of the daemon. Returns the mode to switch to, if any.
fn step(st: &mut State, matching: Vec<String>, winner: Option<&str>, current: Option<&str>) -> Option<String> {
    let changed = matching != st.matching;
    st.matching = matching;
    if st.applied.is_some() && st.applied.as_deref() != current {
        (st.applied, st.previous, st.yielded) = (None, None, true);
    }
    if st.yielded && !changed {
        return None;
    }
    st.yielded = false;
    let target = match winner {
        Some(w) => {
            if st.applied.is_none() {
                st.previous = current.map(String::from);
            }
            st.applied = Some(w.into());
            Some(w.to_string())
        }
        None => {
            st.applied = None;
            st.previous.take()
        }
    };
    target.filter(|t| Some(t.as_str()) != current)
}

fn rules_file() -> Result<PathBuf> {
    Ok(config_home()?.join("osmium/auto.rules"))
}

/// None when there is no rules file.
fn load_rules() -> Result<Option<Vec<Rule>>> {
    let path = rules_file()?;
    match fs::read_to_string(&path) {
        Ok(text) => parse_rules(&text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Where osmium-mode records the active mode.
fn current_mode() -> Result<Option<String>> {
    let text = fs::read_to_string(state_home()?.join("osmium/mode")).ok();
    Ok(text.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

fn monitor_labels(json: &str) -> Vec<String> {
    let field = |m: &BTreeMap<String, String>, k: &str| m.get(k).cloned().unwrap_or_default();
    json_objects(json).iter().map(|m| format!("{} {}", field(m, "name"), field(m, "description"))).collect()
}

/// The connected SSID in `nmcli -t -f active,ssid dev wifi` output.
fn active_ssid(text: &str) -> Option<String> {
    let ssid = text.lines().find_map(|l| l.strip_prefix("yes:")).filter(|s| !s.is_empty())?;
    // Terse mode escapes the field separator and the backslash.
    Some(ssid.replace("\\:", ":").replace("\\\\", "\\"))
}

/// Collects only what the rules ask about: no wifi or compositor queries for nothing.
fn gather(rules: &[Rule]) -> Facts {
    let uses = |pred: fn(&Cond) -> bool| rules.iter().flat_map(|r| &r.conds).any(pred);
    let mut f = Facts::default();
    if uses(|c| matches!(c, Cond::App(_))) {
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(|c: char| c.is_ascii_digit())
                && let Ok(bytes) = fs::read(entry.path().join("comm"))
            {
                f.apps.insert(comm(&bytes));
            }
        }
    }
    if uses(|c| matches!(c, Cond::Screens(_) | Cond::Screen(_)))
        && let Ok(json) = output(hyprctl().args(["monitors", "-j"]))
    {
        f.monitors = monitor_labels(&json);
    }
    if uses(|c| matches!(c, Cond::Wifi(_))) {
        // LC_ALL=C: "yes" is translated otherwise. --rescan no: asking every 5 s must not scan.
        let mut nmcli = Command::new("nmcli");
        nmcli.env("LC_ALL", "C").args(["-t", "-f", "active,ssid", "dev", "wifi", "list", "--rescan", "no"]);
        f.wifi = output(&mut nmcli).ok().and_then(|t| active_ssid(&t));
    }
    if uses(|c| matches!(c, Cond::Time(..))) {
        // std has no local time zone support.
        f.minutes = output(Command::new("date").arg("+%H:%M")).ok().and_then(|t| hhmm(&t));
    }
    for entry in fs::read_dir(POWER_SUPPLY).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with("BAT") {
            let read = |name: &str| fs::read_to_string(entry.path().join(name)).ok();
            f.battery = f.battery.or_else(|| read("capacity")?.trim().parse().ok());
            f.discharging |= read("status").is_some_and(|s| s.trim() == "Discharging");
        }
    }
    f
}

fn report(rules: &[Rule], facts: &Facts) -> String {
    let hits = matching(rules, facts);
    let mut out = String::new();
    for r in rules {
        let mark = match hits.iter().position(|h| h.line == r.line) {
            Some(0) => '*',
            Some(_) => '+',
            None => ' ',
        };
        let conds: Vec<String> =
            r.conds.iter().map(|c| format!("{c} ({})", if c.holds(facts) { "evet" } else { "hayır" })).collect();
        out += &format!("{mark} satır {}: {}: {}\n", r.line, r.mode, conds.join(" ve "));
    }
    out += &match hits.first() {
        Some(r) => format!("Seçilen mod: {} (satır {})\n", r.mode, r.line),
        None => "Eşleşen kural yok.\n".to_string(),
    };
    out
}

fn check() -> Result {
    let rules = load_rules()?.ok_or("kural dosyası yok; örnek için: osmium-auto init")?;
    print!("{}", report(&rules, &gather(&rules)));
    Ok(())
}

fn init() -> Result {
    let path = rules_file()?;
    if path.exists() {
        println!("{} zaten var, dokunulmadı.", path.display());
        return Ok(());
    }
    let err = |e: io::Error| format!("{}: {e}", path.display());
    fs::create_dir_all(path.parent().unwrap()).map_err(err)?;
    fs::write(&path, EXAMPLE).map_err(err)?;
    println!("Örnek kurallar yazıldı: {}", path.display());
    println!("Açmak için: systemctl --user enable --now osmium-auto.service");
    Ok(())
}

fn tick(st: &mut State) -> Result {
    let rules = load_rules()?.unwrap_or_default();
    let facts = gather(&rules);
    let hits = matching(&rules, &facts);
    let keys = hits.iter().map(|r| r.key()).collect();
    let winner = hits.first().map(|r| r.mode.as_str());
    let before = st.to_text();
    let switch = step(st, keys, winner, current_mode()?.as_deref());
    if st.to_text() != before {
        store_state(st)?;
    }
    if let Some(mode) = switch {
        // A failed switch leaves the mode file unchanged, which the next tick
        // reads as a manual override: no retry storm.
        // osmium-mode talks to the compositor too: hand it the instance we found.
        let mut set = Command::new("osmium-mode");
        set.envs(hyprctl().get_envs().filter_map(|(k, v)| Some((k, v?)))).args(["set", &mode]);
        run(&mut set)?;
    }
    Ok(())
}

/// True for a compositor's answer to `hyprctl monitors -j`; hyprctl's own
/// "could not connect" text may come with exit status 0.
fn is_monitor_list(reply: &str) -> bool {
    reply.trim_start().starts_with('[')
}

fn daemon() -> Result {
    // The unit starts with the user manager, before the greetd session execs the
    // compositor. A switch tried then fails, and a failed switch reads as a
    // manual override: the rule would stay unapplied for the whole session.
    while !output(hyprctl().args(["monitors", "-j"])).is_ok_and(|t| is_monitor_list(&t)) {
        std::thread::sleep(Duration::from_secs(5));
    }
    let mut st = load_state()?;
    let mut last_err = String::new();
    loop {
        // The rules file is re-read every tick, so a typo in it must not kill the daemon.
        match tick(&mut st) {
            Ok(()) => last_err.clear(),
            Err(e) if e != last_err => {
                eprintln!("osmium-auto: {e}");
                last_err = e;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(s: &str) -> Cond {
        parse_cond(s).unwrap()
    }

    #[test]
    fn parses_rules() {
        let rules = parse_rules(
            "# comment\n\noyun: uygulama=steam ve priz\nsunum: ekran=2\n\
             calisma: wifi=Ofis Agi ve saat=09:00-18:00 ve pil<30 ve pilde ve ekran=Dell\n",
        )
        .unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], Rule { line: 3, mode: "oyun".into(), conds: vec![Cond::App("steam".into()), Cond::Ac] });
        assert_eq!(rules[1].conds, [Cond::Screens(2)]);
        assert_eq!(
            rules[2].conds,
            [
                Cond::Wifi("Ofis Agi".into()),
                Cond::Time(540, 1080),
                Cond::BatteryBelow(30),
                Cond::OnBattery,
                Cond::Screen("Dell".into())
            ]
        );
    }

    #[test]
    fn parse_errors_name_the_line() {
        for (text, line, what) in [
            ("oyun uygulama=steam", 1, "bekleniyordu"),
            ("# c\noyun: renk=mavi", 2, "bilinmeyen koşul"),
            ("\n\noyun: ", 3, "bilinmeyen koşul"),
            ("oyun: pil<abc", 1, "0-100"),
            ("oyun: pil<101", 1, "0-100"),
            ("oyun: saat=25:00-26:00", 1, "saat="),
            ("oyun: saat=09:00", 1, "saat="),
            ("oyun: wifi=", 1, "değer boş"),
            ("--help: priz", 1, "geçersiz mod adı"),
            ("a/b: priz", 1, "geçersiz mod adı"),
            (": priz", 1, "geçersiz mod adı"),
            ("oyun: priz ve", 1, "bilinmeyen koşul"),
        ] {
            let e = parse_rules(text).unwrap_err();
            assert!(e.contains(&format!("satır {line}:")) && e.contains(what), "{text:?}: {e}");
        }
    }

    #[test]
    fn conditions_display_as_written() {
        for s in ["uygulama=steam", "ekran=2", "ekran=Dell", "wifi=Ev", "pil<20", "saat=22:00-06:30", "priz", "pilde"] {
            assert_eq!(cond(s).to_string(), s);
        }
    }

    #[test]
    fn example_parses() {
        let rules = parse_rules(EXAMPLE).unwrap();
        assert_eq!(rules.iter().map(|r| r.mode.as_str()).collect::<Vec<_>>(), ["oyun", "sunum"]);
    }

    #[test]
    fn app_condition() {
        let f = Facts { apps: ["steam".to_string(), "very-long-proce".to_string()].into(), ..Default::default() };
        assert!(cond("uygulama=steam").holds(&f));
        assert!(!cond("uygulama=stea").holds(&f));
        assert!(cond("uygulama=very-long-process-name").holds(&f), "comm is cut at 15 bytes");
        // The cut lands inside `ö` (bytes 15-16): what the kernel shows must still match.
        let cut = &"abcdefghijklmnö-oyun".as_bytes()[..15];
        let f = Facts { apps: [comm(&[cut, b"\n"].concat())].into(), ..Default::default() };
        assert!(cond("uygulama=abcdefghijklmnö-oyun").holds(&f) && !cond("uygulama=abcdefghijklmn").holds(&f));
        assert_eq!(comm(b"steam\n"), "steam");
    }

    #[test]
    fn screen_condition() {
        let f = Facts { monitors: vec!["eDP-1 BOE 0x0A1D".into(), "HDMI-A-1 Dell Inc. U2720Q".into()], ..Default::default() };
        assert!(cond("ekran=2").holds(&f) && cond("ekran=1").holds(&f) && !cond("ekran=3").holds(&f));
        assert!(cond("ekran=dell").holds(&f) && cond("ekran=HDMI").holds(&f) && !cond("ekran=Samsung").holds(&f));
    }

    #[test]
    fn wifi_condition() {
        let f = Facts { wifi: Some("Ev".into()), ..Default::default() };
        assert!(cond("wifi=Ev").holds(&f) && !cond("wifi=ev").holds(&f));
        assert!(!cond("wifi=Ev").holds(&Facts::default()));
    }

    #[test]
    fn power_conditions() {
        let on_battery = Facts { battery: Some(19), discharging: true, ..Default::default() };
        let charging = Facts { battery: Some(19), ..Default::default() };
        let desktop = Facts::default();
        assert!(cond("pil<20").holds(&on_battery) && !cond("pil<19").holds(&on_battery));
        assert!(!cond("pil<20").holds(&charging) && !cond("pil<20").holds(&desktop));
        assert!(cond("pilde").holds(&on_battery) && !cond("priz").holds(&on_battery));
        assert!(cond("priz").holds(&charging) && cond("priz").holds(&desktop) && !cond("pilde").holds(&desktop));
    }

    #[test]
    fn time_range_wraps_midnight() {
        assert_eq!(hhmm("09:30\n"), Some(570));
        assert_eq!((hhmm("24:00"), hhmm("12:60"), hhmm("12")), (None, None, None));
        assert!(in_range(540, 1080, 540) && in_range(540, 1080, 1079) && !in_range(540, 1080, 1080));
        // 22:00-06:00
        assert!(in_range(1320, 360, 1320) && in_range(1320, 360, 0) && in_range(1320, 360, 359));
        assert!(!in_range(1320, 360, 360) && !in_range(1320, 360, 720));
        let at = |m| Facts { minutes: Some(m), ..Default::default() };
        assert!(cond("saat=22:00-06:00").holds(&at(30)) && !cond("saat=22:00-06:00").holds(&at(720)));
        assert!(!cond("saat=00:00-23:59").holds(&Facts::default()), "unknown time matches nothing");
    }

    #[test]
    fn first_match_wins() {
        let rules = parse_rules("oyun: uygulama=steam\nsunum: ekran=1\nnormal: priz").unwrap();
        let f = Facts { monitors: vec!["eDP-1".into()], ..Default::default() };
        let hits = matching(&rules, &f);
        assert_eq!(hits.iter().map(|r| r.line).collect::<Vec<_>>(), [2, 3]);
        let text = report(&rules, &f);
        assert!(text.contains("  satır 1: oyun: uygulama=steam (hayır)"), "{text}");
        assert!(text.contains("* satır 2: sunum: ekran=1 (evet)") && text.contains("+ satır 3: normal"), "{text}");
        assert!(text.ends_with("Seçilen mod: sunum (satır 2)\n"), "{text}");
        assert!(report(&rules[..1], &f).ends_with("Eşleşen kural yok.\n"));
    }

    fn keys(n: &[usize]) -> Vec<String> {
        n.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn state_survives_a_restart() {
        let mut st = State::default();
        step(&mut st, keys(&[1]), Some("oyun"), Some("normal"));
        let mut back = State::parse(&st.to_text());
        assert_eq!(back, st);
        assert_eq!(step(&mut back, vec![], None, Some("oyun")).as_deref(), Some("normal"));
        let yielded = State { yielded: true, matching: keys(&[2]), ..State::default() };
        assert_eq!(State::parse(&yielded.to_text()), yielded);
        assert_eq!(State::parse("bozuk\napplied=\nmatch=a: b=c\n"), State { matching: vec!["a: b=c".into()], ..State::default() });
    }

    #[test]
    fn rule_identity_ignores_line_numbers() {
        let a = parse_rules("oyun: uygulama=steam ve priz").unwrap();
        let b = parse_rules("# yeni yorum\n\noyun:   uygulama=steam  ve  priz").unwrap();
        assert_ne!(a[0].line, b[0].line);
        assert_eq!(a[0].key(), b[0].key());
        assert_eq!(a[0].key(), "oyun: uygulama=steam ve priz");
    }

    #[test]
    fn takes_over_and_returns() {
        let mut st = State::default();
        assert_eq!(step(&mut st, keys(&[]), None, Some("normal")), None);
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("normal")).as_deref(), Some("oyun"));
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("oyun")), None, "applied once");
        // A higher rule starts matching: switch, but still return to the original mode.
        assert_eq!(step(&mut st, keys(&[1, 2]), Some("oyun"), Some("oyun")), None);
        assert_eq!(step(&mut st, keys(&[2]), Some("sunum"), Some("oyun")).as_deref(), Some("sunum"));
        assert_eq!(step(&mut st, keys(&[]), None, Some("sunum")).as_deref(), Some("normal"));
        assert_eq!(step(&mut st, keys(&[]), None, Some("normal")), None);
        assert_eq!(st, State::default());
    }

    #[test]
    fn does_not_fight_a_manual_switch() {
        let mut st = State::default();
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("normal")).as_deref(), Some("oyun"));
        // The user picks another mode while the rule still matches.
        for _ in 0..3 {
            assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("calisma")), None);
        }
        // The rule stops matching: nothing to undo, the manual mode stays.
        assert_eq!(step(&mut st, keys(&[]), None, Some("calisma")), None);
        // It matches again: take over, and later return to the manual mode.
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("calisma")).as_deref(), Some("oyun"));
        assert_eq!(step(&mut st, keys(&[]), None, Some("oyun")).as_deref(), Some("calisma"));
    }

    #[test]
    fn manual_switch_and_new_match_in_one_tick() {
        let mut st = State::default();
        step(&mut st, keys(&[1]), Some("oyun"), Some("normal"));
        assert_eq!(step(&mut st, keys(&[1, 2]), Some("oyun"), Some("calisma")).as_deref(), Some("oyun"));
        assert_eq!(st.previous.as_deref(), Some("calisma"));
    }

    #[test]
    fn failed_switch_is_not_retried() {
        let mut st = State::default();
        assert!(step(&mut st, keys(&[1]), Some("yok"), Some("normal")).is_some());
        assert_eq!(step(&mut st, keys(&[1]), Some("yok"), Some("normal")), None);
    }

    #[test]
    fn winner_already_active_or_no_previous_mode() {
        let mut st = State::default();
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), Some("oyun")), None);
        assert_eq!(step(&mut st, keys(&[]), None, Some("oyun")), None, "nothing to return to");
        // No mode was ever set: take over, and leave it when the rule ends.
        assert_eq!(step(&mut st, keys(&[1]), Some("oyun"), None).as_deref(), Some("oyun"));
        assert_eq!(step(&mut st, keys(&[]), None, Some("oyun")), None);
    }

    const MONITORS: &str = r#"[{
        "id": 0,
        "name": "eDP-1",
        "description": "BOE \"NE160\" 0x0A1D",
        "width": 2560,
        "refreshRate": 165.00400,
        "x": -1280,
        "activeWorkspace": { "id": 1, "name": "1" },
        "reserved": [0, 40, 0, 0],
        "scale": 1.60,
        "focused": true,
        "availableModes": ["2560x1600@165.00Hz", "2560x1600@60.00Hz"]
    },{
        "id": 1,
        "name": "HDMI-A-1",
        "description": "Dell Inc. DELL U2720Q ç",
        "activeWorkspace": { "id": 2, "name": "web" }
    }]"#;

    #[test]
    fn labels_monitors() {
        assert_eq!(monitor_labels(MONITORS), ["eDP-1 BOE \"NE160\" 0x0A1D", "HDMI-A-1 Dell Inc. DELL U2720Q ç"]);
    }

    #[test]
    fn recognises_a_compositor_reply() {
        assert!(is_monitor_list(MONITORS) && is_monitor_list("\n[]"));
        assert!(!is_monitor_list("") && !is_monitor_list("Couldn't connect to /run/user/1000/hypr/x/.socket.sock"));
    }

    #[test]
    fn finds_active_ssid() {
        assert_eq!(active_ssid("no:Komsu\nyes:Ev Agi\nno:\n").as_deref(), Some("Ev Agi"));
        assert_eq!(active_ssid("yes:a\\:b\\\\c\n").as_deref(), Some("a:b\\c"));
        assert_eq!(active_ssid("no:Komsu\n"), None);
        assert_eq!(active_ssid(""), None);
    }
}
