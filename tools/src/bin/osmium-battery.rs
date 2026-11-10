//! Battery budget: "this charge has to last 4 hours" and the system saves
//! just enough power to get there.
//!
//!   osmium-battery                 percent, power draw, estimated time left
//!   osmium-battery hedef <süre>    set a target runtime (4s, 90d, 4h, 90m) and start the controller
//!   osmium-battery kapat           stop the controller and restore what it changed
//!   osmium-battery run             controller loop (user unit osmium-battery.service)
//!
//! The controller walks a fixed ladder: power-saver profile, dimmer screen,
//! 60 Hz, dimmer still. Whatever it changes is first written to
//! ~/.local/state/osmium/battery/saved, so `kapat` restores even after a crash.

use osmium_tools::{Result, exit_on_err, output, remove_dir, run, state_home, succeeds};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const USAGE: &str = "kullanım: osmium-battery [hedef <süre> | kapat | run]\n  süre: 4s (saat), 90d (dakika), 4h, 90m";
const POWER_SUPPLY: &str = "/sys/class/power_supply";
const UNIT: &str = "osmium-battery.service";
const TICK: u64 = 30;
const MAX_LEVEL: usize = 5;
/// Weight of a new sample in the smoothed draw.
const ALPHA: f64 = 0.3;
/// Ticks to wait after a step, so the smoothed draw reflects it before the next decision.
const SETTLE: u8 = 2;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => status(),
        ["hedef", d] => target(d),
        ["kapat"] => off(),
        ["run"] => controller(),
        _ => Err(USAGE.into()),
    };
    exit_on_err("osmium-battery", res);
}

/// `4s`/`4h` (hours) or `90d`/`90m` (minutes) into seconds.
fn parse_duration(s: &str) -> Result<u64> {
    let err = || format!("geçersiz süre `{s}` (örnek: 4s = 4 saat, 90d = 90 dakika)");
    let unit = s.chars().last().ok_or_else(err)?;
    let n: u64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| err())?;
    let secs = match unit {
        's' | 'h' => n.checked_mul(3600),
        'd' | 'm' => n.checked_mul(60),
        _ => None,
    };
    match secs.ok_or_else(err)? {
        0 => Err("süre sıfır olamaz".into()),
        s if s > 48 * 3600 => Err("süre en fazla 48 saat olabilir".into()),
        s => Ok(s),
    }
}

fn fmt_duration(secs: u64) -> String {
    match (secs / 3600, secs % 3600 / 60) {
        (0, m) => format!("{m} dk"),
        (h, m) => format!("{h} sa {m} dk"),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Battery {
    energy_wh: f64,
    full_wh: f64,
    power_w: f64,
}

impl Battery {
    fn percent(&self) -> u64 {
        if self.full_wh > 0.0 { (self.energy_wh / self.full_wh * 100.0).round().min(100.0) as u64 } else { 0 }
    }
}

/// One battery from its sysfs values (micro units). `get` reads a file of the
/// BAT directory. Some drivers expose energy_*/power_now, others charge_*/current_now.
fn parse_battery(get: impl Fn(&str) -> Option<String>) -> Option<Battery> {
    // current_now and power_now are negative while discharging on some drivers.
    let num = |name: &str| get(name)?.trim().parse::<f64>().ok().map(f64::abs);
    if let (Some(energy), Some(full)) = (num("energy_now"), num("energy_full")) {
        let power = num("power_now").or_else(|| Some(num("current_now")? * num("voltage_now")? / 1e6))?;
        return Some(Battery { energy_wh: energy / 1e6, full_wh: full / 1e6, power_w: power / 1e6 });
    }
    let volts = num("voltage_now")? / 1e6;
    Some(Battery {
        energy_wh: num("charge_now")? / 1e6 * volts,
        full_wh: num("charge_full")? / 1e6 * volts,
        power_w: num("current_now")? / 1e6 * volts,
    })
}

/// All batteries summed, and whether any is discharging. None without a battery.
fn read_batteries() -> Option<(Battery, bool)> {
    let mut found = None;
    for entry in fs::read_dir(POWER_SUPPLY).into_iter().flatten().flatten() {
        if !entry.file_name().to_string_lossy().starts_with("BAT") {
            continue;
        }
        let read = |name: &str| fs::read_to_string(entry.path().join(name)).ok();
        let Some(b) = parse_battery(read) else { continue };
        let (sum, discharging) = found.get_or_insert((Battery::default(), false));
        sum.energy_wh += b.energy_wh;
        sum.full_wh += b.full_wh;
        sum.power_w += b.power_w;
        *discharging |= read("status").is_some_and(|s| s.trim() == "Discharging");
    }
    found
}

fn smooth(prev: Option<f64>, sample: f64) -> f64 {
    prev.map_or(sample, |p| p + ALPHA * (sample - p))
}

/// The dead band between the two thresholds is what keeps the ladder from oscillating.
fn next_level(level: usize, draw: f64, needed: f64) -> usize {
    if draw > needed * 1.05 {
        (level + 1).min(MAX_LEVEL)
    } else if draw < needed * 0.85 {
        level.saturating_sub(1)
    } else {
        level
    }
}

#[derive(Debug, PartialEq)]
struct Plan {
    saver: bool,
    /// Brightness as percent of the value before dimming.
    dim: u64,
    cap_refresh: bool,
}

fn plan(level: usize) -> Plan {
    let dim = match level {
        0 | 1 => 100,
        2 | 3 => 60,
        4 => 35,
        _ => 20,
    };
    Plan { saver: level >= 1, dim, cap_refresh: level >= 3 }
}

/// Never below 10% of the panel's range, and never brighter than the user had it.
/// Rounded up and at least 1: panels with a handful of steps would otherwise go black.
fn dim_target(start: u64, max: u64, pct: u64) -> u64 {
    (start * pct / 100).max(max.div_ceil(10).max(1)).min(start)
}

#[derive(Debug, Default)]
struct Ctl {
    level: usize,
    smoothed: Option<f64>,
    settle: u8,
    notified: bool,
}

impl Ctl {
    /// One reading on battery. Returns the realistic runtime in seconds when
    /// the target cannot be met at the last level (once).
    fn tick(&mut self, energy_wh: f64, power_w: f64, remaining: u64) -> Option<u64> {
        // Some drivers report 0 right after unplugging.
        if power_w > 0.0 {
            self.smoothed = Some(smooth(self.smoothed, power_w));
        }
        let draw = self.smoothed?;
        if self.settle > 0 {
            self.settle -= 1;
            return None;
        }
        let needed = energy_wh * 3600.0 / remaining.max(1) as f64;
        let new = next_level(self.level, draw, needed);
        if new != self.level {
            (self.level, self.settle) = (new, SETTLE);
        } else if self.level == MAX_LEVEL && draw > needed * 1.05 && !self.notified {
            self.notified = true;
            return Some((energy_wh / draw * 3600.0) as u64);
        }
        None
    }
}

/// Original values of what is currently changed; an entry leaves when it is restored.
#[derive(Debug, Default, PartialEq)]
struct Saved {
    profile: Option<String>,
    brightness: Option<u64>,
    /// `hyprctl keyword monitor` values that bring the refresh rates back.
    monitors: Vec<String>,
}

impl Saved {
    /// Lenient, and strict about values: they become command arguments.
    fn parse(text: &str) -> Saved {
        let mut s = Saved::default();
        let safe = |v: &str, extra: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
        for line in text.lines() {
            match line.split_once('=') {
                Some(("profile", v)) if safe(v, "-") => s.profile = Some(v.into()),
                Some(("brightness", v)) => s.brightness = v.parse().ok(),
                Some(("monitor", v)) if safe(v, "-_.,@") => s.monitors.push(v.into()),
                _ => {}
            }
        }
        s
    }

    fn to_text(&self) -> String {
        let mut out = String::new();
        out.extend(self.profile.iter().map(|p| format!("profile={p}\n")));
        out.extend(self.brightness.iter().map(|b| format!("brightness={b}\n")));
        out.extend(self.monitors.iter().map(|m| format!("monitor={m}\n")));
        out
    }

    fn load() -> Result<Saved> {
        Ok(Saved::parse(&fs::read_to_string(state_dir()?.join("saved")).unwrap_or_default()))
    }

    fn store(&self) {
        warn(write_state("saved", &self.to_text()));
    }

    fn is_empty(&self) -> bool {
        *self == Saved::default()
    }
}

fn state_dir() -> Result<PathBuf> {
    Ok(state_home()?.join("osmium/battery"))
}

fn write_state(name: &str, text: &str) -> Result {
    let dir = state_dir()?;
    fs::create_dir_all(&dir).and_then(|()| fs::write(dir.join(name), text)).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Seconds of runtime still to cover, if a target is set.
fn read_target() -> Result<Option<u64>> {
    Ok(fs::read_to_string(state_dir()?.join("target")).ok().and_then(|s| s.trim().parse().ok()))
}

/// The savers are optional hardware/services: a missing one must not stop the rest.
fn warn(res: Result) {
    if let Err(e) = res {
        eprintln!("osmium-battery: uyarı: {e}");
    }
}

/// A user unit starts outside the compositor's environment, so hyprctl would
/// not find the instance: point it at the newest one.
fn hyprctl(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("hyprctl");
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        let dirs = std::env::var_os("XDG_RUNTIME_DIR").and_then(|d| fs::read_dir(PathBuf::from(d).join("hypr")).ok());
        let newest = dirs.into_iter().flatten().flatten().max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        if let Some(e) = newest {
            cmd.env("HYPRLAND_INSTANCE_SIGNATURE", e.file_name());
        }
    }
    output(cmd.args(args))
}

/// hyprctl exits 0 even when the compositor refuses, and then prints the reason instead of `ok`.
fn set_monitor(spec: &str) -> Result {
    match hyprctl(&["keyword", "monitor", spec])?.trim() {
        "ok" => Ok(()),
        other => Err(format!("hyprctl keyword monitor {spec}: {other}")),
    }
}

/// Scalar fields of each object in a top-level JSON array, as text. Nested
/// values are skipped: enough for `hyprctl monitors -j`, and there is no JSON crate.
// shortcut: same scanner as osmium-auto.rs; move it to lib.rs when that file is free to edit.
fn json_objects(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut out: Vec<BTreeMap<String, String>> = Vec::new();
    let mut depth = 0;
    let mut key: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let value = match c {
            '{' | '[' => {
                depth += 1;
                key = None;
                if c == '{' && depth == 2 {
                    out.push(BTreeMap::new());
                }
                continue;
            }
            '}' | ']' => {
                depth -= 1;
                continue;
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => s.extend(chars.next()),
                        c => s.push(c),
                    }
                }
                s
            }
            c if c.is_ascii_alphanumeric() || c == '-' => {
                let mut s = String::from(c);
                while let Some(n) = chars.next_if(|n| !matches!(n, ',' | '}' | ']') && !n.is_whitespace()) {
                    s.push(n);
                }
                s
            }
            _ => continue,
        };
        if depth == 2 {
            match (key.take(), out.last_mut()) {
                (Some(k), Some(object)) => {
                    object.insert(k, value);
                }
                _ => key = Some(value),
            }
        }
    }
    out
}

/// (60 Hz value, restore value) for `hyprctl keyword monitor`, per monitor running above 60 Hz.
fn monitor_specs(json: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for m in json_objects(json) {
        let field = |k: &str| m.get(k).map(String::as_str).unwrap_or_default();
        let name = field("name");
        let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let rate: f64 = field("refreshRate").parse().unwrap_or(0.0);
        // 61: 60.01 Hz panels are already there.
        if !named || rate <= 61.0 || field("disabled") == "true" {
            continue;
        }
        // A missing field would give a spec the compositor rejects or misreads.
        if ["width", "height", "x", "y", "scale"].iter().any(|k| field(k).is_empty()) {
            continue;
        }
        // A monitor keyword without transform resets a rotated screen.
        let transform = match field("transform") {
            "" | "0" => String::new(),
            t => format!(",transform,{t}"),
        };
        let spec = |hz: &str| {
            let (w, h, x, y, scale) = (field("width"), field("height"), field("x"), field("y"), field("scale"));
            format!("{name},{w}x{h}@{hz},{x}x{y},{scale}{transform}")
        };
        out.push((spec("60"), spec(field("refreshRate"))));
    }
    out
}

fn set_saver(on: bool, saved: &mut Saved) {
    if !on {
        if let Some(p) = saved.profile.take() {
            warn(run(Command::new("powerprofilesctl").args(["set", &p])));
            saved.store();
        }
        return;
    }
    if saved.profile.is_none() {
        match output(Command::new("powerprofilesctl").arg("get")) {
            // Only what Saved::parse accepts back: an unrestorable profile must not be replaced.
            Ok(p) if Saved::parse(&format!("profile={}", p.trim())).profile.is_some() => {
                saved.profile = Some(p.trim().to_string())
            }
            Ok(p) => return warn(Err(format!("powerprofilesctl get: `{}`", p.trim()))),
            Err(e) => return warn(Err(e)),
        }
        saved.store();
    }
    warn(run(Command::new("powerprofilesctl").args(["set", "power-saver"])));
}

/// `-c backlight`: without a class brightnessctl may pick an LED (keyboard, caps lock).
fn brightnessctl() -> Command {
    let mut cmd = Command::new("brightnessctl");
    cmd.args(["-c", "backlight"]);
    cmd
}

fn brightness(args: &[&str]) -> Result<u64> {
    let text = output(brightnessctl().args(args))?;
    text.trim().parse().map_err(|_| format!("brightnessctl {}: `{}`", args.join(" "), text.trim()))
}

fn set_brightness(value: u64) {
    warn(run(brightnessctl().args(["-q", "set", &value.to_string()])));
}

fn set_dim(pct: u64, saved: &mut Saved) {
    if pct >= 100 {
        if let Some(b) = saved.brightness.take() {
            set_brightness(b);
            saved.store();
        }
        return;
    }
    if saved.brightness.is_none() {
        match brightness(&["get"]) {
            Ok(b) => saved.brightness = Some(b),
            Err(e) => return warn(Err(e)),
        }
        saved.store();
    }
    match (saved.brightness, brightness(&["max"])) {
        (Some(start), Ok(max)) => set_brightness(dim_target(start, max, pct)),
        (_, Err(e)) => warn(Err(e)),
        _ => {}
    }
}

fn set_cap(on: bool, saved: &mut Saved) {
    if !on {
        for spec in std::mem::take(&mut saved.monitors) {
            warn(set_monitor(&spec));
        }
        return saved.store();
    }
    match hyprctl(&["monitors", "-j"]) {
        Ok(json) => {
            for (cap, restore) in monitor_specs(&json) {
                saved.monitors.push(restore);
                saved.store();
                warn(set_monitor(&cap));
            }
        }
        Err(e) => warn(Err(e)),
    }
}

fn apply(old: usize, new: usize, saved: &mut Saved) {
    let (old, new) = (plan(old), plan(new));
    if old.saver != new.saver {
        set_saver(new.saver, saved);
    }
    if old.dim != new.dim {
        set_dim(new.dim, saved);
    }
    if old.cap_refresh != new.cap_refresh {
        set_cap(new.cap_refresh, saved);
    }
}

fn restore(saved: &mut Saved) {
    set_saver(false, saved);
    set_dim(100, saved);
    set_cap(false, saved);
}

fn status() -> Result {
    let (b, discharging) = read_batteries().ok_or("pil bulunamadı")?;
    println!("Pil: %{}", b.percent());
    if discharging {
        println!("Tüketim: {:.1} W", b.power_w);
        if b.power_w > 0.0 {
            println!("Tahmini kalan süre: {}", fmt_duration((b.energy_wh / b.power_w * 3600.0) as u64));
        }
    } else {
        println!("Prizde");
    }
    if let Some(left) = read_target()? {
        println!("Hedef: {} daha dayanmalı", fmt_duration(left));
    }
    Ok(())
}

fn target(duration: &str) -> Result {
    let secs = parse_duration(duration)?;
    let (_, discharging) = read_batteries().ok_or("pil bulunamadı")?;
    // Stop first: a running controller writes its own countdown to the same file.
    succeeds(Command::new("systemctl").args(["--user", "stop", UNIT]));
    write_state("target", &secs.to_string())?;
    run(Command::new("systemctl").args(["--user", "start", UNIT]))?;
    println!("Hedef: {}. Kapatmak için: osmium-battery kapat", fmt_duration(secs));
    if !discharging {
        println!("Şu an prizde; süre fişi çekince işlemeye başlar.");
    }
    Ok(())
}

fn off() -> Result {
    // Stop first: a running controller would re-apply its level.
    succeeds(Command::new("systemctl").args(["--user", "stop", UNIT]));
    restore(&mut Saved::load()?);
    remove_dir(state_dir()?)?;
    println!("Pil hedefi kapatıldı, ayarlar geri yüklendi.");
    Ok(())
}

fn notify(title: &str, body: &str) {
    warn(run(Command::new("notify-send").args(["-a", "osmium-battery", title, body])));
}

fn controller() -> Result {
    let mut remaining = read_target()?.ok_or("hedef yok; önce: osmium-battery hedef <süre>")?;
    let mut saved = Saved::load()?;
    // Left over from a crash: start from the user's own settings.
    restore(&mut saved);
    let mut ctl = Ctl::default();
    while remaining > 0 {
        let Some((b, discharging)) = read_batteries() else {
            restore(&mut saved);
            return Err("pil bulunamadı".into());
        };
        if discharging {
            let old = ctl.level;
            if let Some(estimate) = ctl.tick(b.energy_wh, b.power_w, remaining) {
                let body = format!(
                    "En tasarruflu ayarda bile tahmini süre {} (hedef: {}).",
                    fmt_duration(estimate),
                    fmt_duration(remaining)
                );
                notify("Pil hedefi tutmayacak", &body);
            }
            apply(old, ctl.level, &mut saved);
        } else if ctl.level > 0 || !saved.is_empty() {
            restore(&mut saved);
            ctl = Ctl::default();
        }
        // sleep does not advance while suspended, so only time awake on battery counts.
        std::thread::sleep(Duration::from_secs(TICK));
        if discharging {
            remaining = remaining.saturating_sub(TICK);
            warn(write_state("target", &remaining.to_string()));
        }
    }
    restore(&mut saved);
    remove_dir(state_dir()?)?;
    notify("Pil hedefi tamamlandı", "Ayarlar geri yüklendi.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("4s"), Ok(14400));
        assert_eq!(parse_duration("4h"), Ok(14400));
        assert_eq!(parse_duration("90d"), Ok(5400));
        assert_eq!(parse_duration("90m"), Ok(5400));
        for bad in ["", "4", "s", "0s", "-1s", "1.5s", "4x", "49s", "4ş", "99999999999999999999s", "18446744073709551615s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn formats_estimates() {
        assert_eq!(fmt_duration(0), "0 dk");
        assert_eq!(fmt_duration(59 * 60 + 59), "59 dk");
        assert_eq!(fmt_duration(3600), "1 sa 0 dk");
        assert_eq!(fmt_duration(3 * 3600 + 20 * 60), "3 sa 20 dk");
    }

    fn sysfs(values: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| values.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn reads_energy_layout() {
        let b = parse_battery(sysfs(&[("energy_now", "40000000\n"), ("energy_full", "50000000\n"), ("power_now", "12500000\n")]))
            .unwrap();
        assert!(close(b.energy_wh, 40.0) && close(b.full_wh, 50.0) && close(b.power_w, 12.5), "{b:?}");
        assert_eq!(b.percent(), 80);
    }

    #[test]
    fn reads_charge_layout() {
        let b = parse_battery(sysfs(&[
            ("charge_now", "3000000\n"),
            ("charge_full", "4000000\n"),
            ("voltage_now", "11400000\n"),
            ("current_now", "-1000000\n"),
        ]))
        .unwrap();
        assert!(close(b.energy_wh, 34.2) && close(b.full_wh, 45.6) && close(b.power_w, 11.4), "{b:?}");
        assert_eq!(b.percent(), 75);
    }

    #[test]
    fn energy_layout_without_power_now() {
        let b = parse_battery(sysfs(&[
            ("energy_now", "40000000"),
            ("energy_full", "50000000"),
            ("current_now", "1000000"),
            ("voltage_now", "12000000"),
        ]))
        .unwrap();
        assert!(close(b.power_w, 12.0), "{b:?}");
    }

    #[test]
    fn incomplete_battery_is_skipped() {
        assert_eq!(parse_battery(sysfs(&[("energy_now", "40000000")])), None);
        assert_eq!(parse_battery(sysfs(&[("charge_now", "x"), ("voltage_now", "1")])), None);
        assert_eq!(Battery::default().percent(), 0);
    }

    #[test]
    fn smooths_the_draw() {
        assert_eq!(smooth(None, 10.0), 10.0);
        assert!(close(smooth(Some(10.0), 20.0), 13.0));
        // A spike moves the average only partly; a steady value converges.
        let mut s = 10.0;
        for _ in 0..30 {
            s = smooth(Some(s), 20.0);
        }
        assert!((s - 20.0).abs() < 0.01);
    }

    #[test]
    fn ladder_has_a_dead_band() {
        assert_eq!(next_level(2, 10.6, 10.0), 3);
        assert_eq!(next_level(2, 10.4, 10.0), 2);
        assert_eq!(next_level(2, 8.6, 10.0), 2);
        assert_eq!(next_level(2, 8.4, 10.0), 1);
        assert_eq!(next_level(MAX_LEVEL, 99.0, 10.0), MAX_LEVEL);
        assert_eq!(next_level(0, 1.0, 10.0), 0);
    }

    #[test]
    fn ladder_plans() {
        assert_eq!(plan(0), Plan { saver: false, dim: 100, cap_refresh: false });
        assert_eq!(plan(1), Plan { saver: true, dim: 100, cap_refresh: false });
        assert_eq!(plan(2), Plan { saver: true, dim: 60, cap_refresh: false });
        assert_eq!(plan(3), Plan { saver: true, dim: 60, cap_refresh: true });
        assert_eq!((plan(4).dim, plan(5).dim), (35, 20));
    }

    #[test]
    fn brightness_floor() {
        assert_eq!(dim_target(1000, 1000, 60), 600);
        assert_eq!(dim_target(400, 1000, 20), 100, "not below 10% of max");
        assert_eq!(dim_target(50, 1000, 60), 50, "never brighter than the user had it");
        // A 7-step panel: 3 * 20% rounds to 0, and max / 10 would too.
        assert_eq!(dim_target(3, 7, 20), 1, "never black");
        assert_eq!(dim_target(15, 15, 20), 3);
    }

    #[test]
    fn controller_steps_and_settles() {
        let mut c = Ctl::default();
        // 40 Wh for 4 h needs 10 W; drawing 15 W.
        assert_eq!(c.tick(40.0, 15.0, 4 * 3600), None);
        assert_eq!((c.level, c.settle), (1, SETTLE));
        c.tick(40.0, 15.0, 4 * 3600);
        c.tick(40.0, 15.0, 4 * 3600);
        assert_eq!(c.level, 1, "waits for the step to show");
        c.tick(40.0, 15.0, 4 * 3600);
        assert_eq!(c.level, 2);
        // Inside the dead band: stays.
        c = Ctl { level: 2, ..Default::default() };
        c.tick(40.0, 9.0, 4 * 3600);
        assert_eq!(c.level, 2);
        // Well below: one step back.
        c = Ctl { level: 2, ..Default::default() };
        c.tick(40.0, 5.0, 4 * 3600);
        assert_eq!(c.level, 1);
    }

    #[test]
    fn controller_notifies_once_at_the_last_level() {
        let mut c = Ctl { level: MAX_LEVEL, ..Default::default() };
        assert_eq!(c.tick(40.0, 20.0, 4 * 3600), Some(2 * 3600));
        assert_eq!(c.tick(40.0, 20.0, 4 * 3600), None);
        assert_eq!(c.level, MAX_LEVEL);
    }

    #[test]
    fn controller_ignores_zero_power() {
        let mut c = Ctl::default();
        assert_eq!(c.tick(40.0, 0.0, 3600), None);
        assert_eq!((c.level, c.smoothed), (0, None));
    }

    #[test]
    fn saved_round_trips_and_rejects_odd_values() {
        let s = Saved {
            profile: Some("balanced".into()),
            brightness: Some(1234),
            monitors: vec!["eDP-1,2560x1600@165.00400,0x0,1.60".into(), "DP-2,1920x1080@144.0,-1920x0,1.00,transform,1".into()],
        };
        assert_eq!(Saved::parse(&s.to_text()), s);
        assert!(Saved::parse("profile=a b\nbrightness=x\nmonitor=eDP-1;rm\nmonitor=\njunk\n").is_empty());
        // set_saver relies on this to refuse an empty `powerprofilesctl get` reply.
        assert!(Saved::parse("profile=").is_empty() && Saved::parse("profile=power-saver").profile.is_some());
    }

    const MONITORS: &str = r#"[{
        "id": 0,
        "name": "eDP-1",
        "description": "BOE \"NE160\"",
        "width": 2560,
        "height": 1600,
        "refreshRate": 165.00400,
        "x": 0,
        "y": 0,
        "activeWorkspace": { "id": 1, "name": "1" },
        "reserved": [0, 40, 0, 0],
        "scale": 1.60,
        "transform": 0,
        "disabled": false,
        "availableModes": ["2560x1600@165.00Hz", "2560x1600@60.00Hz"]
    },{
        "id": 1,
        "name": "HDMI-A-1",
        "width": 3840,
        "height": 2160,
        "refreshRate": 60.00000,
        "x": 2560,
        "y": 0,
        "scale": 2.00,
        "transform": 0
    },{
        "id": 2,
        "name": "DP-2",
        "width": 1920,
        "height": 1080,
        "refreshRate": 144.00101,
        "x": -1920,
        "y": 0,
        "activeWorkspace": { "id": 3, "name": "3" },
        "scale": 1.00,
        "transform": 1
    }]"#;

    #[test]
    fn caps_only_fast_monitors() {
        assert_eq!(
            monitor_specs(MONITORS),
            [
                ("eDP-1,2560x1600@60,0x0,1.60".to_string(), "eDP-1,2560x1600@165.00400,0x0,1.60".to_string()),
                (
                    "DP-2,1920x1080@60,-1920x0,1.00,transform,1".to_string(),
                    "DP-2,1920x1080@144.00101,-1920x0,1.00,transform,1".to_string()
                ),
            ]
        );
        assert!(monitor_specs("").is_empty() && monitor_specs("[]").is_empty());
        assert!(monitor_specs(r#"[{"name": "a,b", "refreshRate": 144.0}]"#).is_empty());
        assert!(monitor_specs(r#"[{"name": "DP-1", "refreshRate": 144.0, "width": 1920, "height": 1080}]"#).is_empty());
        // What we save must survive our own validation on load.
        let saved = Saved { monitors: monitor_specs(MONITORS).into_iter().map(|(_, r)| r).collect(), ..Default::default() };
        assert_eq!(Saved::parse(&saved.to_text()), saved);
    }
}
