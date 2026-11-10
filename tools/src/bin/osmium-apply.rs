//! Declarative system file: describe packages, desktop, services and basic
//! system settings in one TOML file and bring the machine in line with it.
//!
//!   osmium-apply plan [file]     show what would change, change nothing
//!   osmium-apply [file]          show the plan, ask, then apply it
//!   osmium-apply export [file]   write a file describing this machine
//!
//! The file defaults to ~/.config/osmium/osmium.toml. Only a small TOML subset
//! is read (tables, "strings", true/false, arrays of strings); anything else is
//! an error with its line number, so a typo never becomes a root command.

use osmium_tools::{Result, config_home, exit_on_err, output, run, succeeds};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "kullanım: osmium-apply [dosya] | plan [dosya] | export [dosya]";
/// Packages the ISO installs; `export` leaves them out when the file exists.
const BASE_PACKAGES: &str = "/usr/share/osmium/base-packages.txt";
const UNIT_SUFFIXES: &[&str] = &[".service", ".socket", ".timer", ".target", ".path", ".mount"];

// ---------- TOML subset ----------

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    fn show(&self) -> String {
        match self {
            Value::Str(s) => format!("\"{s}\""),
            Value::Bool(b) => b.to_string(),
            Value::List(_) => "dizi".into(),
        }
    }
}

/// Keys of one table with the line each was defined on.
type Table = BTreeMap<String, (usize, Value)>;
/// Tables by name with the line of their header; top-level keys are under "".
type Doc = BTreeMap<String, (usize, Table)>;

struct Parser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

impl Parser {
    fn err<T>(&self, msg: &str) -> Result<T> {
        Err(format!("satır {}: {msg}", self.line))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) {
        if self.peek() == Some('\n') {
            self.line += 1;
        }
        self.pos += 1;
    }

    /// Skips blanks and comments, and line breaks too when `newlines` is set.
    fn skip(&mut self, newlines: bool) {
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' => self.bump(),
                '\n' if newlines => self.bump(),
                '#' => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn word(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-') {
            s.push(c);
            self.bump();
        }
        s
    }

    fn string(&mut self) -> Result<String> {
        if self.peek() != Some('"') {
            return self.err("tırnak içinde metin bekleniyordu");
        }
        self.bump();
        let mut s = String::new();
        loop {
            match self.peek() {
                Some('"') => {
                    self.bump();
                    return Ok(s);
                }
                Some('\\') => return self.err("metin içinde ters eğik çizgi desteklenmiyor"),
                Some('\n') | None => return self.err("kapanmamış tırnak"),
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
    }

    fn value(&mut self) -> Result<Value> {
        match self.peek() {
            Some('"') => self.string().map(Value::Str),
            Some('[') => {
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip(true);
                    if self.peek() == Some(']') {
                        self.bump();
                        return Ok(Value::List(items));
                    }
                    items.push(self.string()?);
                    self.skip(true);
                    match self.peek() {
                        Some(',') => self.bump(),
                        Some(']') => {}
                        _ => return self.err("dizide `,` veya `]` bekleniyordu"),
                    }
                }
            }
            _ => match self.word().as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => self.err("desteklenmeyen değer (yalnızca \"metin\", true/false ve [\"a\", \"b\"] okunur)"),
            },
        }
    }
}

fn parse_toml(text: &str) -> Result<Doc> {
    let mut p = Parser { chars: text.chars().collect(), pos: 0, line: 1 };
    let mut doc = Doc::new();
    let mut section = String::new();
    loop {
        p.skip(true);
        let Some(c) = p.peek() else { return Ok(doc) };
        if c == '[' {
            p.bump();
            section = p.word();
            if section.is_empty() || p.peek() != Some(']') {
                return p.err("bölüm başlığı `[ad]` biçiminde olmalı");
            }
            p.bump();
            if doc.insert(section.clone(), (p.line, Table::new())).is_some() {
                return p.err(&format!("[{section}] iki kez tanımlanmış"));
            }
        } else {
            let line = p.line;
            let key = p.word();
            p.skip(false);
            if key.is_empty() || p.peek() != Some('=') {
                return p.err("`anahtar = değer` bekleniyordu");
            }
            p.bump();
            p.skip(false);
            let value = p.value()?;
            let table = &mut doc.entry(section.clone()).or_insert((line, Table::new())).1;
            if table.insert(key.clone(), (line, value)).is_some() {
                return Err(format!("satır {line}: `{key}` iki kez tanımlanmış"));
            }
        }
        p.skip(false);
        if !matches!(p.peek(), None | Some('\n')) {
            return p.err("satır sonunda beklenmeyen metin");
        }
    }
}

// ---------- validation: every value below can reach a root command ----------

fn safe(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with(['-', '.', '/'])
        && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

fn valid_pkg(s: &str) -> bool {
    safe(s, "@._+-")
}

fn valid_unit(s: &str) -> bool {
    safe(s, ":_.@-") && UNIT_SUFFIXES.iter().any(|x| s.len() > x.len() && s.ends_with(x))
}

fn valid_id(s: &str) -> bool {
    safe(s, "_-")
}

fn valid_hostname(s: &str) -> bool {
    s.len() <= 63 && safe(s, "-") && !s.ends_with('-')
}

fn valid_timezone(s: &str) -> bool {
    safe(s, "_+-/") && !s.ends_with('/') && !s.contains("//")
}

fn valid_locale(s: &str) -> bool {
    safe(s, "_.@-")
}

#[derive(Debug, Default, PartialEq)]
struct Config {
    install: Vec<String>,
    remove: Vec<String>,
    dots: Option<String>,
    mode: Option<String>,
    enable: Vec<String>,
    disable: Vec<String>,
    user_enable: Vec<String>,
    hostname: Option<String>,
    timezone: Option<String>,
    locale: Option<String>,
}

fn take_str(t: &mut Table, key: &str, valid: fn(&str) -> bool, what: &str) -> Result<Option<String>> {
    match t.remove(key) {
        None => Ok(None),
        Some((_, Value::Str(s))) if valid(&s) => Ok(Some(s)),
        Some((line, Value::Str(s))) => Err(format!("satır {line}: geçersiz {what}: \"{s}\"")),
        Some((line, other)) => Err(format!("satır {line}: `{key}` tırnak içinde metin olmalı, bulunan: {}", other.show())),
    }
}

fn take_list(t: &mut Table, key: &str, valid: fn(&str) -> bool, what: &str) -> Result<Vec<String>> {
    match t.remove(key) {
        None => Ok(Vec::new()),
        Some((line, Value::List(items))) => match items.iter().find(|s| !valid(s)) {
            Some(bad) => Err(format!("satır {line}: `{key}` içinde geçersiz {what}: \"{bad}\"")),
            None => Ok(items),
        },
        Some((line, other)) => Err(format!("satır {line}: `{key}` metin dizisi olmalı, bulunan: {}", other.show())),
    }
}

fn to_config(mut doc: Doc) -> Result<Config> {
    let mut cfg = Config::default();
    let mut section = |name: &str, fill: &mut dyn FnMut(&mut Table) -> Result| -> Result {
        let Some((_, mut t)) = doc.remove(name) else { return Ok(()) };
        fill(&mut t)?;
        match t.iter().next() {
            Some((key, (line, _))) => Err(format!("satır {line}: [{name}] içinde bilinmeyen anahtar `{key}`")),
            None => Ok(()),
        }
    };
    section("paketler", &mut |t| {
        cfg.install = take_list(t, "kur", valid_pkg, "paket adı")?;
        cfg.remove = take_list(t, "kaldir", valid_pkg, "paket adı")?;
        Ok(())
    })?;
    section("masaustu", &mut |t| {
        cfg.dots = take_str(t, "dots", valid_id, "dots kimliği")?;
        cfg.mode = take_str(t, "mod", valid_id, "mod kimliği")?;
        Ok(())
    })?;
    section("servisler", &mut |t| {
        cfg.enable = take_list(t, "ac", valid_unit, "birim adı")?;
        cfg.disable = take_list(t, "kapat", valid_unit, "birim adı")?;
        cfg.user_enable = take_list(t, "kullanici_ac", valid_unit, "birim adı")?;
        Ok(())
    })?;
    section("sistem", &mut |t| {
        cfg.hostname = take_str(t, "hostname", valid_hostname, "makine adı")?;
        cfg.timezone = take_str(t, "timezone", valid_timezone, "saat dilimi")?;
        cfg.locale = take_str(t, "locale", valid_locale, "yerel ayar")?;
        Ok(())
    })?;
    if let Some((name, (line, table))) = doc.into_iter().next() {
        return Err(match table.iter().next() {
            Some((key, (line, _))) if name.is_empty() => format!("satır {line}: `{key}` bir bölümün içinde olmalı"),
            _ => format!("satır {line}: bilinmeyen bölüm [{name}]"),
        });
    }
    if let Some(p) = cfg.install.iter().find(|p| cfg.remove.contains(p)) {
        return Err(format!("`{p}` hem kur hem kaldir listesinde"));
    }
    if let Some(u) = cfg.enable.iter().find(|u| cfg.disable.contains(u)) {
        return Err(format!("`{u}` hem ac hem kapat listesinde"));
    }
    Ok(cfg)
}

fn parse_config(text: &str) -> Result<Config> {
    to_config(parse_toml(text)?)
}

// ---------- plan ----------

/// What the machine looks like now, as far as the file cares.
#[derive(Debug, Default)]
struct Facts {
    installed: BTreeSet<String>,
    enabled: BTreeSet<String>,
    user_enabled: BTreeSet<String>,
    dots: String,
    mode: String,
    hostname: String,
    timezone: String,
    locale: String,
}

#[derive(Debug, PartialEq)]
enum Action {
    Install(Vec<String>),
    Remove(Vec<String>),
    Enable(Vec<String>),
    Disable(Vec<String>),
    UserEnable(Vec<String>),
    Dots(String),
    Mode(String),
    Hostname(String),
    Timezone(String),
    /// Reported only: changing the locale needs locale-gen and a new session.
    LocaleDiffers(String),
}

fn plan(cfg: &Config, f: &Facts) -> Vec<Action> {
    let pick = |items: &[String], keep: &dyn Fn(&String) -> bool| -> Vec<String> {
        items.iter().filter(|x| keep(x)).cloned().collect()
    };
    let differs = |want: &Option<String>, have: &str| want.clone().filter(|w| w != have);
    let mut actions = Vec::new();
    let mut list = |items: Vec<String>, make: fn(Vec<String>) -> Action| {
        if !items.is_empty() {
            actions.push(make(items));
        }
    };
    list(pick(&cfg.install, &|p| !f.installed.contains(p)), Action::Install);
    list(pick(&cfg.remove, &|p| f.installed.contains(p)), Action::Remove);
    list(pick(&cfg.enable, &|u| !f.enabled.contains(u)), Action::Enable);
    list(pick(&cfg.disable, &|u| f.enabled.contains(u)), Action::Disable);
    list(pick(&cfg.user_enable, &|u| !f.user_enabled.contains(u)), Action::UserEnable);
    actions.extend(differs(&cfg.dots, &f.dots).map(Action::Dots));
    actions.extend(differs(&cfg.mode, &f.mode).map(Action::Mode));
    actions.extend(differs(&cfg.hostname, &f.hostname).map(Action::Hostname));
    actions.extend(differs(&cfg.timezone, &f.timezone).map(Action::Timezone));
    actions.extend(differs(&cfg.locale, &f.locale).map(Action::LocaleDiffers));
    actions
}

impl Action {
    fn describe(&self) -> String {
        match self {
            Action::Install(p) => format!("kurulacak paketler: {}", p.join(" ")),
            Action::Remove(p) => format!("kaldırılacak paketler (artık gerekmeyen bağımlılıklarıyla): {}", p.join(" ")),
            Action::Enable(u) => format!("açılacak servisler: {}", u.join(" ")),
            Action::Disable(u) => format!("kapatılacak servisler: {}", u.join(" ")),
            Action::UserEnable(u) => format!("açılacak kullanıcı servisleri: {}", u.join(" ")),
            Action::Dots(d) => format!("masaüstü (dots): {d}"),
            Action::Mode(m) => format!("mod: {m}"),
            Action::Hostname(h) => format!("makine adı: {h}"),
            Action::Timezone(t) => format!("saat dilimi: {t}"),
            Action::LocaleDiffers(l) => format!("yerel ayar {l} isteniyor ama UYGULANMAZ (elle ayarlayın)"),
        }
    }

    /// The command that performs the action; `None` for report-only entries.
    fn argv(&self) -> Option<Vec<String>> {
        let cmd = |head: &[&str], tail: &[String]| Some(head.iter().map(|s| s.to_string()).chain(tail.iter().cloned()).collect());
        let one = std::slice::from_ref::<String>;
        match self {
            Action::Install(p) => cmd(&["sudo", "pacman", "-S", "--needed", "--noconfirm", "--"], p),
            // No --noconfirm: -s also removes orphaned dependencies the plan cannot list.
            Action::Remove(p) => cmd(&["sudo", "pacman", "-Rns", "--"], p),
            Action::Enable(u) => cmd(&["sudo", "systemctl", "enable", "--now", "--"], u),
            Action::Disable(u) => cmd(&["sudo", "systemctl", "disable", "--now", "--"], u),
            Action::UserEnable(u) => cmd(&["systemctl", "--user", "enable", "--now", "--"], u),
            Action::Dots(d) => cmd(&["osmium-dots", "apply"], one(d)),
            Action::Mode(m) => cmd(&["osmium-mode", "set"], one(m)),
            Action::Hostname(h) => cmd(&["sudo", "hostnamectl", "set-hostname"], one(h)),
            Action::Timezone(t) => cmd(&["sudo", "timedatectl", "set-timezone"], one(t)),
            Action::LocaleDiffers(_) => None,
        }
    }
}

// ---------- system access ----------

fn trimmed(cmd: &mut Command) -> String {
    output(cmd).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `LANG=` from /etc/locale.conf.
fn parse_lang(text: &str) -> String {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("LANG="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default()
}

fn gather(cfg: &Config) -> Result<Facts> {
    let enabled = |user: bool, units: &mut dyn Iterator<Item = &String>| -> BTreeSet<String> {
        units
            .filter(|u| {
                let mut c = Command::new("systemctl");
                if user {
                    c.arg("--user");
                }
                succeeds(c.args(["is-enabled", "--", u]))
            })
            .cloned()
            .collect()
    };
    Ok(Facts {
        installed: output(Command::new("pacman").arg("-Qq"))?.lines().map(str::to_string).collect(),
        enabled: enabled(false, &mut cfg.enable.iter().chain(&cfg.disable)),
        user_enabled: enabled(true, &mut cfg.user_enable.iter()),
        dots: trimmed(Command::new("osmium-dots").arg("current")),
        mode: trimmed(Command::new("osmium-mode").arg("current")),
        hostname: trimmed(Command::new("hostnamectl").arg("hostname")),
        timezone: trimmed(Command::new("timedatectl").args(["show", "-p", "Timezone", "--value"])),
        locale: parse_lang(&fs::read_to_string("/etc/locale.conf").unwrap_or_default()),
    })
}

fn default_file(arg: Option<&str>) -> Result<PathBuf> {
    match arg {
        Some(p) => Ok(PathBuf::from(p)),
        None => Ok(config_home()?.join("osmium/osmium.toml")),
    }
}

fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Prints the plan and returns it.
fn show_plan(path: &Path) -> Result<Vec<Action>> {
    let cfg = load(path)?;
    let actions = plan(&cfg, &gather(&cfg)?);
    if actions.is_empty() {
        println!("Sistem dosyayla uyumlu, yapılacak bir şey yok.");
    }
    for a in &actions {
        println!("  {}", a.describe());
    }
    Ok(actions)
}

fn apply(path: &Path) -> Result {
    let actions = show_plan(path)?;
    let commands: Vec<(String, Vec<String>)> = actions.iter().filter_map(|a| Some((a.describe(), a.argv()?))).collect();
    if commands.is_empty() {
        return Ok(());
    }
    print!("\nBu değişiklikler uygulansın mı? Onaylamak için 'evet' yazın: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    if answer.trim() != "evet" {
        println!("Vazgeçildi, hiçbir şey değiştirilmedi.");
        return Ok(());
    }
    for (what, argv) in commands {
        println!("\n> {what}");
        run(Command::new(&argv[0]).args(&argv[1..]))?;
    }
    println!("\nTamamlandı.");
    Ok(())
}

// ---------- export ----------

/// Explicitly installed packages that the ISO did not ship.
fn without_base(explicit: &str, base: &str) -> Vec<String> {
    let base: BTreeSet<&str> = base.lines().map(str::trim).collect();
    explicit.lines().map(str::trim).filter(|p| valid_pkg(p) && !base.contains(p)).map(str::to_string).collect()
}

/// Units from `systemctl list-unit-files --state=enabled` that the user turned
/// on: enabled although the vendor preset says disabled.
fn preset_disabled(list_unit_files: &str) -> Vec<String> {
    list_unit_files
        .lines()
        .filter_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [unit, "enabled", "disabled"] if valid_unit(unit) => Some(unit.to_string()),
            _ => None,
        })
        .collect()
}

fn export_text(cfg: &Config) -> String {
    let list = |key: &str, items: &[String]| {
        let body: String = items.iter().map(|i| format!("  \"{i}\",\n")).collect();
        format!("{key} = [\n{body}]\n")
    };
    let string = |key: &str, v: &Option<String>| v.as_ref().map(|v| format!("{key} = \"{v}\"\n")).unwrap_or_default();
    format!(
        "# osmium-apply export ile üretildi. Uygulamak için: osmium-apply\n\n\
         [paketler]\n{}\n[masaustu]\n{}{}\n[servisler]\n{}{}\n[sistem]\n{}{}{}",
        list("kur", &cfg.install),
        string("dots", &cfg.dots),
        string("mod", &cfg.mode),
        list("ac", &cfg.enable),
        list("kullanici_ac", &cfg.user_enable),
        string("hostname", &cfg.hostname),
        string("timezone", &cfg.timezone),
        string("locale", &cfg.locale),
    )
}

fn export(path: &Path) -> Result {
    if path.exists() {
        return Err(format!("{} zaten var; üzerine yazılmaz. Başka bir dosya adı verin.", path.display()));
    }
    let unit_files = |user: bool| {
        let mut c = Command::new("systemctl");
        if user {
            c.arg("--user");
        }
        output(c.args(["list-unit-files", "--state=enabled", "--no-legend", "--plain"]))
    };
    // Values that would not pass validation are left out, so the file always loads.
    let checked = |v: String, valid: fn(&str) -> bool| Some(v).filter(|v| valid(v));
    let cfg = Config {
        install: without_base(
            &output(Command::new("pacman").arg("-Qqe"))?,
            &fs::read_to_string(BASE_PACKAGES).unwrap_or_default(),
        ),
        dots: checked(trimmed(Command::new("osmium-dots").arg("current")), valid_id),
        mode: checked(trimmed(Command::new("osmium-mode").arg("current")), valid_id),
        enable: preset_disabled(&unit_files(false)?),
        user_enable: preset_disabled(&unit_files(true).unwrap_or_default()),
        hostname: checked(trimmed(Command::new("hostnamectl").arg("hostname")), valid_hostname),
        timezone: checked(trimmed(Command::new("timedatectl").args(["show", "-p", "Timezone", "--value"])), valid_timezone),
        locale: checked(parse_lang(&fs::read_to_string("/etc/locale.conf").unwrap_or_default()), valid_locale),
        ..Default::default()
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, export_text(&cfg)).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{} yazıldı ({} paket, {} servis).", path.display(), cfg.install.len(), cfg.enable.len() + cfg.user_enable.len());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let file = |arg: &[&str]| default_file(arg.first().copied());
    let res = match args[..] {
        [flag, ..] if flag.starts_with('-') => Err(USAGE.into()),
        ["plan", ref f @ ..] if f.len() <= 1 => file(f).and_then(|p| show_plan(&p).map(|_| ())),
        ["export", ref f @ ..] if f.len() <= 1 => file(f).and_then(|p| export(&p)),
        ref f if f.len() <= 1 => file(f).and_then(|p| apply(&p)),
        _ => Err(USAGE.into()),
    };
    exit_on_err("osmium-apply", res);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const SAMPLE: &str = "# my machine\n\
        [paketler]\n\
        kur = [\"firefox\", \"gtk4\", # browser\n  \"libc++\",\n]\n\
        kaldir = []\n\n\
        [masaustu]\n\
        dots = \"ii\"   # default desktop\n\
        mod = \"normal\"\n\
        [servisler]\n\
        ac = [\"bluetooth.service\"]\n\
        kapat = [\"cups.socket\"]\n\
        kullanici_ac = [\"osmium-history.timer\"]\n\
        [sistem]\n\
        hostname = \"osmium-pc\"\n\
        timezone = \"Europe/Istanbul\"\n\
        locale = \"tr_TR.UTF-8\"";

    #[test]
    fn parses_sample() {
        let cfg = parse_config(SAMPLE).unwrap();
        assert_eq!(cfg.install, ["firefox", "gtk4", "libc++"]);
        assert!(cfg.remove.is_empty());
        assert_eq!((cfg.dots.as_deref(), cfg.mode.as_deref()), (Some("ii"), Some("normal")));
        assert_eq!(cfg.enable, ["bluetooth.service"]);
        assert_eq!(cfg.disable, ["cups.socket"]);
        assert_eq!(cfg.user_enable, ["osmium-history.timer"]);
        assert_eq!(cfg.timezone.as_deref(), Some("Europe/Istanbul"));
        assert_eq!(cfg.locale.as_deref(), Some("tr_TR.UTF-8"));
        assert_eq!(parse_config("").unwrap(), Config::default());
    }

    #[test]
    fn toml_values_and_lines() {
        let doc = parse_toml("a = true\n[x]\nb = false # c\nc = \"# not a comment\"\r\n").unwrap();
        assert_eq!(doc[""].1["a"], (1, Value::Bool(true)));
        assert_eq!(doc["x"].1["b"], (3, Value::Bool(false)));
        assert_eq!(doc["x"].1["c"], (4, Value::Str("# not a comment".into())));
    }

    #[test]
    fn rejects_unsupported_toml_with_line() {
        for (text, line) in [
            ("[paketler]\nkur = 5", 2),
            ("[paketler]\nkur = 'a'", 2),
            ("\n\n[a.b]", 3),
            ("[[paketler]]", 1),
            ("x = \"a\" y = \"b\"", 1),
            ("x = \"open", 1),
            ("x = \"a\\n\"", 1),
            ("x = [\"a\" \"b\"]", 1),
            ("x = [\n\"a\",\n[\"b\"]]", 3),
            ("x = [true]", 1),
            ("x = { a = \"b\" }", 1),
            ("x \"a\"", 1),
            ("x = \"a\"\nx = \"b\"", 2),
            ("[a]\n[a]", 2),
        ] {
            let err = parse_toml(text).unwrap_err();
            assert!(err.starts_with(&format!("satır {line}:")), "{text:?}: {err}");
        }
    }

    #[test]
    fn rejects_unknown_and_mistyped_keys() {
        assert!(parse_config("[paketler]\nkurr = []").unwrap_err().contains("satır 2: [paketler] içinde bilinmeyen anahtar `kurr`"));
        assert!(parse_config("\n[paket]\n").unwrap_err().contains("satır 2: bilinmeyen bölüm [paket]"));
        assert!(parse_config("dots = \"ii\"").unwrap_err().contains("satır 1"));
        assert!(parse_config("[paketler]\nkur = \"firefox\"").unwrap_err().contains("metin dizisi"));
        assert!(parse_config("[masaustu]\ndots = true").unwrap_err().contains("bulunan: true"));
        assert!(parse_config("[paketler]\nkur = [\"a\"]\nkaldir = [\"a\"]").unwrap_err().contains("hem kur hem kaldir"));
    }

    #[test]
    fn rejects_values_unsafe_for_root_commands() {
        for bad in [
            "[paketler]\nkur = [\"--overwrite=*\"]",
            "[paketler]\nkur = [\"a b\"]",
            "[paketler]\nkaldir = [\"a;reboot\"]",
            "[paketler]\nkur = [\"\"]",
            "[servisler]\nac = [\"bluetooth\"]",
            "[servisler]\nac = [\"--root=/x.service\"]",
            "[servisler]\nkapat = [\"../x.service\"]",
            "[masaustu]\ndots = \"github:evil/repo\"",
            "[masaustu]\nmod = \"-x\"",
            "[sistem]\nhostname = \"a b\"",
            "[sistem]\nhostname = \"-pc\"",
            "[sistem]\ntimezone = \"../../etc/passwd\"",
            "[sistem]\ntimezone = \"/etc/localtime\"",
            "[sistem]\nlocale = \"tr_TR UTF-8\"",
        ] {
            assert!(parse_config(bad).unwrap_err().contains("satır 2: "), "{bad}");
        }
        assert!(valid_unit("getty@tty1.service") && !valid_unit(".service"));
        assert!(valid_timezone("Etc/GMT+3") && valid_timezone("UTC"));
    }

    fn facts() -> Facts {
        Facts {
            installed: ["gtk4", "cups", "nano"].iter().map(|s| s.to_string()).collect(),
            enabled: ["cups.socket"].iter().map(|s| s.to_string()).collect(),
            user_enabled: BTreeSet::new(),
            dots: "synthwave".into(),
            mode: "normal".into(),
            hostname: "osmium-pc".into(),
            timezone: "UTC".into(),
            locale: "en_US.UTF-8".into(),
        }
    }

    #[test]
    fn plan_lists_only_differences() {
        let mut cfg = parse_config(SAMPLE).unwrap();
        cfg.remove = strings(&["nano", "vim"]);
        assert_eq!(
            plan(&cfg, &facts()),
            [
                Action::Install(strings(&["firefox", "libc++"])),
                Action::Remove(strings(&["nano"])),
                Action::Enable(strings(&["bluetooth.service"])),
                Action::Disable(strings(&["cups.socket"])),
                Action::UserEnable(strings(&["osmium-history.timer"])),
                Action::Dots("ii".into()),
                Action::Timezone("Europe/Istanbul".into()),
                Action::LocaleDiffers("tr_TR.UTF-8".into()),
            ]
        );
    }

    #[test]
    fn plan_is_empty_when_in_sync() {
        let cfg = parse_config("[paketler]\nkur = [\"nano\"]\nkaldir = [\"vim\"]\n[masaustu]\nmod = \"normal\"\n[servisler]\nac = [\"cups.socket\"]").unwrap();
        assert!(plan(&cfg, &facts()).is_empty());
        assert!(plan(&Config::default(), &facts()).is_empty());
    }

    #[test]
    fn action_commands() {
        let argv = |a: Action| a.argv().unwrap().join(" ");
        assert_eq!(argv(Action::Install(strings(&["a", "b"]))), "sudo pacman -S --needed --noconfirm -- a b");
        assert_eq!(argv(Action::Remove(strings(&["a"]))), "sudo pacman -Rns -- a");
        assert_eq!(argv(Action::Enable(strings(&["x.service"]))), "sudo systemctl enable --now -- x.service");
        assert_eq!(argv(Action::Disable(strings(&["x.service"]))), "sudo systemctl disable --now -- x.service");
        assert_eq!(argv(Action::UserEnable(strings(&["x.timer"]))), "systemctl --user enable --now -- x.timer");
        assert_eq!(argv(Action::Dots("ii".into())), "osmium-dots apply ii");
        assert_eq!(argv(Action::Mode("oyun".into())), "osmium-mode set oyun");
        assert_eq!(argv(Action::Hostname("pc".into())), "sudo hostnamectl set-hostname pc");
        assert_eq!(argv(Action::Timezone("UTC".into())), "sudo timedatectl set-timezone UTC");
        assert_eq!(Action::LocaleDiffers("x".into()).argv(), None);
        assert!(Action::LocaleDiffers("x".into()).describe().contains("UYGULANMAZ"));
    }

    #[test]
    fn export_helpers() {
        assert_eq!(without_base("base\nfirefox\nlinux\nsteam\n", "base\n linux \n"), ["firefox", "steam"]);
        assert_eq!(without_base("a\nb\n", ""), ["a", "b"]);
        let units = "bluetooth.service enabled disabled\nsystemd-timesyncd.service enabled enabled\n\
                     getty@.service enabled enabled\nfoo.timer enabled disabled\nweird enabled disabled\nx.service enabled -\n";
        assert_eq!(preset_disabled(units), ["bluetooth.service", "foo.timer"]);
        assert_eq!(parse_lang("# c\nLANG=tr_TR.UTF-8\nLC_TIME=x\n"), "tr_TR.UTF-8");
        assert_eq!(parse_lang(""), "");
    }

    #[test]
    fn export_roundtrips() {
        let cfg = Config {
            install: strings(&["firefox", "steam"]),
            dots: Some("ii".into()),
            mode: None,
            enable: strings(&["bluetooth.service"]),
            hostname: Some("pc".into()),
            timezone: Some("Europe/Istanbul".into()),
            locale: Some("tr_TR.UTF-8".into()),
            ..Default::default()
        };
        let text = export_text(&cfg);
        assert!(text.contains("[paketler]\nkur = [\n  \"firefox\",\n  \"steam\",\n]\n"), "{text}");
        assert!(text.contains("kullanici_ac = [\n]\n") && !text.contains("mod ="), "{text}");
        assert_eq!(parse_config(&text).unwrap(), cfg);
    }
}
