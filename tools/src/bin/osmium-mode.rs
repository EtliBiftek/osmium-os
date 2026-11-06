//! Switches desktop modes: one key changes Hyprland settings, power profile,
//! notifications, idle lock and launched apps together.
//!
//!   osmium-mode list | current | set <mode> | menu
//!
//! Modes are `<id>.mode` files in ~/.config/osmium/modes (yours win) and
//! /usr/share/osmium/modes (defaults). Every switch starts from a clean
//! `hyprctl reload`, so modes never stack.

use osmium_tools::{Result, config_home, exit_on_err, run, state_home, succeeds};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SYSTEM_MODES: &str = "/usr/share/osmium/modes";
const ANYRUN_STDIN: &str = "/usr/lib/anyrun/libstdin.so";

#[derive(Debug, Default, PartialEq)]
struct Mode {
    id: String,
    name: String,
    power: Option<String>,
    notifications: Option<bool>,
    idle: Option<bool>,
    hypr: Vec<String>,
    exec: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["list"] => list(),
        ["current"] => current().map(|c| println!("{}", c.unwrap_or_default())),
        ["set", id] => set(id),
        ["menu"] => menu(),
        _ => Err("usage: osmium-mode list | current | set <mode> | menu".into()),
    };
    exit_on_err("osmium-mode", res);
}

fn parse(id: &str, text: &str) -> Result<Mode> {
    let mut m = Mode { id: id.into(), name: id.into(), ..Default::default() };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |msg: &str| format!("{id}.mode line {}: {msg}", n + 1);
        let (key, value) = line.split_once('=').ok_or_else(|| err("expected `key = value`"))?;
        let (key, value) = (key.trim(), value.trim().to_string());
        let on_off = |v: &str| match v {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err(err("expected on or off")),
        };
        match key {
            "name" => m.name = value,
            "power" => m.power = Some(value),
            "notifications" => m.notifications = Some(on_off(&value)?),
            "idle" => m.idle = Some(on_off(&value)?),
            "hypr" => m.hypr.push(value),
            "exec" => m.exec.push(value),
            _ => return Err(err(&format!("unknown key `{key}`"))),
        }
    }
    Ok(m)
}

/// All modes by id; user files override system defaults.
fn load() -> Result<BTreeMap<String, Mode>> {
    let mut modes = BTreeMap::new();
    for dir in [PathBuf::from(SYSTEM_MODES), config_home()?.join("osmium/modes")] {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|e| e == "mode") {
                let id = path.file_stem().unwrap().to_string_lossy().into_owned();
                let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                modes.insert(id.clone(), parse(&id, &text)?);
            }
        }
    }
    Ok(modes)
}

fn state_file() -> Result<PathBuf> {
    Ok(state_home()?.join("osmium/mode"))
}

fn current() -> Result<Option<String>> {
    Ok(fs::read_to_string(state_file()?).ok().map(|s| s.trim().to_string()))
}

fn list() -> Result {
    let cur = current()?;
    for m in load()?.values() {
        let mark = if cur.as_deref() == Some(&m.id) { "*" } else { " " };
        println!("{mark} {:<12} {}", m.id, m.name);
    }
    Ok(())
}

/// Lua long-bracket string, so commands need no escaping.
fn lua_str(s: &str) -> String {
    let mut eq = String::new();
    while s.contains(&format!("]{eq}]")) {
        eq.push('=');
    }
    format!("[{eq}[{s}]{eq}]")
}

fn hypr_eval(code: &str) -> Result {
    run(Command::new("hyprctl").args(["eval", code]))
}

fn hypr_exec(cmd: &str) -> Result {
    hypr_eval(&format!("hl.exec_cmd({})", lua_str(cmd)))
}

fn set(id: &str) -> Result {
    let modes = load()?;
    let m = modes.get(id).ok_or_else(|| format!("unknown mode `{id}` (see: osmium-mode list)"))?;

    run(Command::new("hyprctl").arg("reload"))?;
    for code in &m.hypr {
        hypr_eval(code)?;
    }
    // Optional services: warn and keep going, the rest of the mode still applies.
    if let Some(p) = &m.power {
        warn(run(Command::new("powerprofilesctl").args(["set", p])));
    }
    if let Some(on) = m.notifications {
        warn(run(Command::new("swaync-client").arg(if on { "--dnd-off" } else { "--dnd-on" })));
    }
    match m.idle {
        Some(true) => hypr_exec("pidof hypridle || hypridle")?,
        Some(false) => {
            succeeds(Command::new("pkill").args(["-x", "hypridle"]));
        }
        None => {}
    }
    for cmd in &m.exec {
        hypr_exec(cmd)?;
    }

    let state = state_file()?;
    fs::create_dir_all(state.parent().unwrap()).map_err(|e| format!("{}: {e}", state.display()))?;
    fs::write(&state, &m.id).map_err(|e| format!("{}: {e}", state.display()))?;
    warn(run(Command::new("hyprctl").args(["notify", "1", "2000", "0", &format!("Mod: {}", m.name)])));
    Ok(())
}

fn warn(res: Result) {
    if let Err(e) = res {
        eprintln!("osmium-mode: warning: {e}");
    }
}

/// Picks a mode with anyrun (stdin plugin) and applies it.
fn menu() -> Result {
    let modes = load()?;
    let mut child = Command::new("anyrun")
        .args(["--plugins", ANYRUN_STDIN])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("anyrun: {e}"))?;
    let names: String = modes.values().map(|m| format!("{}\n", m.name)).collect();
    child.stdin.take().unwrap().write_all(names.as_bytes()).map_err(|e| format!("anyrun: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("anyrun: {e}"))?;
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match modes.values().find(|m| m.name == picked) {
        Some(m) => set(&m.id),
        None => Ok(()), // closed without choosing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys() {
        let m = parse(
            "oyun",
            "# comment\nname = Oyun modu\npower = performance\nnotifications = off\nidle = off\n\
             hypr = hl.config({ animations = { enabled = false } })\nexec = steam\nexec = discord\n",
        )
        .unwrap();
        assert_eq!(m.name, "Oyun modu");
        assert_eq!(m.power.as_deref(), Some("performance"));
        assert_eq!((m.notifications, m.idle), (Some(false), Some(false)));
        assert_eq!(m.hypr, ["hl.config({ animations = { enabled = false } })"]);
        assert_eq!(m.exec, ["steam", "discord"]);
    }

    #[test]
    fn value_may_contain_equals() {
        let m = parse("x", "hypr = hl.config({ general = { gaps_in = 0 } })").unwrap();
        assert_eq!(m.hypr, ["hl.config({ general = { gaps_in = 0 } })"]);
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(parse("x", "nonsense").unwrap_err().contains("line 1"));
        assert!(parse("x", "idle = maybe").is_err());
        assert!(parse("x", "colour = red").unwrap_err().contains("unknown key"));
    }

    #[test]
    fn lua_str_never_closes_early() {
        assert_eq!(lua_str("steam"), "[[steam]]");
        assert_eq!(lua_str("a]]b"), "[=[a]]b]=]");
        assert_eq!(lua_str("a]]b]=]c"), "[==[a]]b]=]c]==]");
    }

    #[test]
    fn default_modes_parse() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data/modes");
        let mut n = 0;
        for e in fs::read_dir(dir).unwrap().flatten() {
            let id = e.path().file_stem().unwrap().to_string_lossy().into_owned();
            parse(&id, &fs::read_to_string(e.path()).unwrap()).unwrap();
            n += 1;
        }
        assert!(n >= 4);
    }
}
