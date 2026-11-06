//! Share a desktop setup as one link, try someone else's with automatic rollback.
//!
//!   osmium-share export [name]      write <name>.osmium-theme from your ~/.config
//!   osmium-share publish <file>     upload it as a public GitHub gist, print the link
//!   osmium-share show <link|file>   list files, risky lines and what would change
//!   osmium-share try <link|file>    apply, reload, keep only if you confirm within 30 s
//!   osmium-share undo               put back what the last `try` replaced
//!
//! A theme can run code: hyprland.lua is Lua, modes and bars start programs.
//! `show`/`try` print every line that can start a program before asking.
//! Only text files under ALLOWED dirs are ever read or written.

use osmium_tools::{Result, config_home, exit_on_err, output, state_home, succeeds};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

const MAGIC: &str = "osmium-share 1";
/// ~/.config subtrees a theme may contain.
const ALLOWED: &[&str] = &["hypr", "ironbar", "anyrun", "alacritty", "swaync", "osmium/modes"];
const MAX_FILE: usize = 1 << 20;
const MAX_TOTAL: usize = 5 << 20;
const MAX_FILES: usize = 200;
const TRY_SECONDS: u64 = 30;
/// Substrings that mean "this line can start a program": Lua calls, hyprlang/mode exec,
/// ironbar (on_click, show_if, {{...}} dynamic strings, poll), hypridle listeners, anyrun plugins.
const RISKY: &[&str] = &[
    "exec", "os.", "io.", "require", "dofile", "loadfile", "load(", "loadstring",
    "on_click", "on-click", "show_if", "{{", "poll", "on-timeout", "on-resume", "on_timeout", "on_resume",
    ".so", "cmd", "command", "script", "shell",
];

#[derive(Debug, PartialEq)]
enum Entry {
    File(String, String),
    /// Undo files only: the path did not exist before.
    Absent(String),
}

#[derive(Debug, PartialEq)]
struct Bundle {
    name: String,
    entries: Vec<Entry>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match args[..] {
        ["export"] => export("my-theme"),
        ["export", name] => export(name),
        ["publish", file] => publish(file),
        ["show", src] => fetch(src).and_then(|b| preview(&b)),
        ["try", src] => try_theme(src),
        ["undo"] => undo(),
        _ => Err("usage: osmium-share export [name] | publish <file> | show <src> | try <src> | undo".into()),
    };
    exit_on_err("osmium-share", res);
}

// ---------- format ----------

fn encode(b: &Bundle) -> String {
    let mut s = format!("{MAGIC}\nname: {}\n", b.name);
    for e in &b.entries {
        match e {
            Entry::File(path, body) => s += &format!("=== file {} {path}\n{body}\n", body.len()),
            Entry::Absent(path) => s += &format!("=== absent {path}\n"),
        }
    }
    s
}

fn decode(text: &str, allow_absent: bool) -> Result<Bundle> {
    let rest = text.strip_prefix(MAGIC).and_then(|r| r.strip_prefix('\n')).ok_or("not an osmium theme")?;
    let (line, mut rest) = rest.split_once('\n').ok_or("truncated theme")?;
    let name = line.strip_prefix("name: ").ok_or("missing theme name")?.trim().to_string();
    let (mut entries, mut total) = (Vec::new(), 0);
    while !rest.is_empty() {
        let (header, after) = rest.split_once('\n').ok_or("truncated theme")?;
        // `absent` has no length field: split it in 3 so a path with spaces stays whole.
        let n = if header.starts_with("=== absent ") { 3 } else { 4 };
        let parts: Vec<&str> = header.splitn(n, ' ').collect();
        match parts[..] {
            ["===", "file", len, path] => {
                let len: usize = len.parse().map_err(|_| format!("bad length in `{header}`"))?;
                check_path(path)?;
                if len > MAX_FILE {
                    return Err(format!("{path}: larger than {MAX_FILE} bytes"));
                }
                total += len;
                let body = after.get(..len).ok_or_else(|| format!("{path}: truncated"))?;
                rest = after[len..].strip_prefix('\n').ok_or_else(|| format!("{path}: bad file end"))?;
                entries.push(Entry::File(path.into(), body.into()));
            }
            ["===", "absent", path] if allow_absent => {
                check_path(path)?;
                rest = after;
                entries.push(Entry::Absent(path.into()));
            }
            _ => return Err(format!("unexpected line `{header}`")),
        }
        if entries.len() > MAX_FILES || total > MAX_TOTAL {
            return Err("theme is too big".into());
        }
    }
    Ok(Bundle { name, entries })
}

/// Relative, no `..`, inside an ALLOWED subtree.
fn check_path(path: &str) -> Result {
    let p = Path::new(path);
    let plain = p.components().all(|c| matches!(c, Component::Normal(_)));
    let allowed = ALLOWED.iter().any(|dir| p.starts_with(dir) && p != Path::new(dir));
    if plain && allowed && !path.contains('\\') {
        Ok(())
    } else {
        Err(format!("refusing path `{path}`: themes may only contain files under {}", ALLOWED.join(", ")))
    }
}

fn risky_lines(b: &Bundle) -> Vec<(String, usize, String)> {
    let mut v = Vec::new();
    for e in &b.entries {
        if let Entry::File(path, body) = e {
            for (i, line) in body.lines().enumerate() {
                let t = line.trim();
                // `--[[x]] os.execute(...)` is a comment followed by code: only plain `--` lines are skipped.
                let comment = (t.starts_with("--") && !t.starts_with("--[")) || t.starts_with('#') || t.starts_with("//");
                if !comment && RISKY.iter().any(|r| t.contains(r)) {
                    v.push((path.clone(), i + 1, t.to_string()));
                }
            }
        }
    }
    v
}

/// gist.github.com/<user>/<id> -> raw content of its first file.
fn raw_url(url: &str) -> String {
    match url.strip_prefix("https://gist.github.com/") {
        Some(rest) => format!("https://gist.githubusercontent.com/{}/raw", rest.trim_end_matches('/')),
        None => url.to_string(),
    }
}

fn fetch(src: &str) -> Result<Bundle> {
    let text = if src.starts_with("https://") {
        output(Command::new("curl").args(["-fsSL", "--max-time", "30", "--max-filesize", "6000000", &raw_url(src)]))?
    } else if src.starts_with("http://") {
        return Err("refusing plain http; use an https link".into());
    } else {
        fs::read_to_string(src).map_err(|e| format!("{src}: {e}"))?
    };
    decode(&text, false)
}

// ---------- commands ----------

fn collect(dir: &Path, rel: &str, out: &mut Vec<Entry>) -> Result {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let (path, rel) = (e.path(), format!("{rel}/{name}"));
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            collect(&path, &rel, out)?;
        } else if meta.is_file() && (meta.len() as usize) <= MAX_FILE {
            match fs::read_to_string(&path) {
                Ok(body) => out.push(Entry::File(rel, body)),
                Err(_) => eprintln!("skipping {rel}: not a text file"),
            }
        }
    }
    Ok(())
}

fn export(name: &str) -> Result {
    let config = config_home()?;
    let mut entries = Vec::new();
    for dir in ALLOWED {
        collect(&config.join(dir), dir, &mut entries)?;
    }
    let b = Bundle { name: name.into(), entries };
    let text = encode(&b);
    decode(&text, false)?; // same limits as the receiving side
    let file = format!("{name}.osmium-theme");
    fs::write(&file, text).map_err(|e| format!("{file}: {e}"))?;
    for e in &b.entries {
        if let Entry::File(p, _) = e {
            println!("  {p}");
        }
    }
    println!("{} dosya -> {file}\nDosyaları kontrol et: paylaşılan her şey herkese açık olur.", b.entries.len());
    Ok(())
}

fn confirm(question: &str) -> bool {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "evet" | "yes")
}

fn publish(file: &str) -> Result {
    let b = decode(&fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?, false)?;
    if !confirm(&format!("'{}' ({} dosya) herkese açık bir GitHub gist olarak yüklenecek. Devam için 'evet' yaz:", b.name, b.entries.len())) {
        return Ok(());
    }
    let url = output(Command::new("gh").args(["gist", "create", "--public", "-d", &format!("OsmiumOS theme: {}", b.name), file]))?;
    println!("Link: {}\nUygulamak için: osmium-share try {}", url.trim(), url.trim());
    Ok(())
}

fn preview(b: &Bundle) -> Result {
    let config = config_home()?;
    println!("Tema: {}", b.name);
    for e in &b.entries {
        if let Entry::File(path, body) = e {
            let status = match fs::read_to_string(config.join(path)) {
                Ok(cur) if cur == *body => "aynı",
                Ok(_) => "değişecek",
                Err(_) => "yeni",
            };
            println!("  {status:<10} {path}");
        }
    }
    let risky = risky_lines(b);
    if !risky.is_empty() {
        println!("\nProgram çalıştırabilen satırlar (dikkatle oku):");
        for (path, n, line) in &risky {
            println!("  {path}:{n}: {line}");
        }
    }
    // Lua can hide a call from any substring scan (`_G["o".."s"]`), so name every Lua file.
    for e in &b.entries {
        if let Entry::File(path, _) = e {
            if path.ends_with(".lua") {
                println!("  {path}: Lua kodu, tamamı çalışır; satır listesi eksik olabilir");
            }
        }
    }
    Ok(())
}

fn undo_file() -> Result<PathBuf> {
    Ok(state_home()?.join("osmium/share-undo.osmium-theme"))
}

/// Writes the bundle into ~/.config and returns what it replaced.
fn write_bundle(b: &Bundle) -> Result<Bundle> {
    let config = config_home()?;
    let mut before = Vec::new();
    for e in &b.entries {
        let (path, body) = match e {
            Entry::File(p, body) => (p, Some(body)),
            Entry::Absent(p) => (p, None),
        };
        let target = config.join(path);
        before.push(match fs::read_to_string(&target) {
            Ok(cur) => Entry::File(path.clone(), cur),
            Err(_) => Entry::Absent(path.clone()),
        });
        match body {
            Some(body) => {
                fs::create_dir_all(target.parent().unwrap()).map_err(|e| format!("{}: {e}", target.display()))?;
                fs::write(&target, body).map_err(|e| format!("{}: {e}", target.display()))?;
            }
            None => {
                let _ = fs::remove_file(&target);
            }
        }
    }
    Ok(Bundle { name: format!("before {}", b.name), entries: before })
}

fn reload() {
    if !succeeds(Command::new("hyprctl").arg("reload")) {
        eprintln!("osmium-share: warning: hyprctl reload failed");
    }
}

fn try_theme(src: &str) -> Result {
    let b = fetch(src)?;
    preview(&b)?;
    if !confirm("\nBu tema uygulanacak ve yukarıdaki satırlar çalışabilir. Devam için 'evet' yaz:") {
        return Ok(());
    }
    // Belt and braces: the config history keeps a copy too.
    succeeds(Command::new("osmium-history").args(["snapshot", &format!("before theme {}", b.name)]));
    let before = write_bundle(&b)?;
    let undo = undo_file()?;
    fs::create_dir_all(undo.parent().unwrap()).map_err(|e| format!("{}: {e}", undo.display()))?;
    fs::write(&undo, encode(&before)).map_err(|e| format!("{}: {e}", undo.display()))?;
    reload();

    print!("Beğendin mi? Kalıcı yapmak için {TRY_SECONDS} saniye içinde 'evet' yaz: ");
    let _ = std::io::stdout().flush();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let _ = tx.send(line);
    });
    let keep = matches!(rx.recv_timeout(Duration::from_secs(TRY_SECONDS)).as_deref().map(str::trim), Ok("evet" | "yes"));
    if keep {
        println!("Tema kalıcı. Geri almak için: osmium-share undo");
    } else {
        println!("\nGeri alınıyor...");
        write_bundle(&before)?;
        reload();
    }
    Ok(())
}

fn undo() -> Result {
    let path = undo_file()?;
    let text = fs::read_to_string(&path).map_err(|_| "nothing to undo")?;
    write_bundle(&decode(&text, true)?)?;
    fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    reload();
    println!("Önceki ayarlar geri yüklendi.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bundle {
        Bundle {
            name: "nord".into(),
            entries: vec![
                Entry::File("hypr/hyprland.lua".into(), "-- c\nhl.exec_cmd(\"waybar\")\n=== file 3 x\n".into()),
                Entry::File("alacritty/alacritty.toml".into(), String::new()),
                Entry::File("osmium/modes/x.mode".into(), "name = X\nexec = steam\nşğü".into()),
            ],
        }
    }

    #[test]
    fn roundtrip_even_with_marker_like_content() {
        let b = sample();
        assert_eq!(decode(&encode(&b), false).unwrap(), b);
    }

    #[test]
    fn rejects_escaping_paths() {
        for bad in ["../.ssh/id_ed25519", "/etc/passwd", "hypr/../../.bashrc", "systemd/user/x.service", "hypr", "hypr\\..\\x"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
        assert!(check_path("hypr/hyprland.lua").is_ok());
        assert!(check_path("osmium/modes/oyun.mode").is_ok());
        assert!(check_path("osmium/other").is_err());
    }

    #[test]
    fn shared_themes_cannot_delete() {
        let b = Bundle { name: "x".into(), entries: vec![Entry::Absent("hypr/hyprland.lua".into())] };
        assert!(decode(&encode(&b), false).is_err());
        assert_eq!(decode(&encode(&b), true).unwrap(), b);
    }

    #[test]
    fn rejects_garbage_and_lies() {
        assert!(decode("hello", false).is_err());
        assert!(decode(&format!("{MAGIC}\nname: x\n=== file 999 hypr/a\nshort\n"), false).is_err());
        assert!(decode(&format!("{MAGIC}\nname: x\n=== file 2 hypr/a\nabc\n"), false).is_err());
    }

    #[test]
    fn flags_program_starting_lines_not_comments() {
        let r = risky_lines(&sample());
        let lines: Vec<&str> = r.iter().map(|(_, _, l)| l.as_str()).collect();
        assert!(lines.contains(&"hl.exec_cmd(\"waybar\")"));
        assert!(lines.contains(&"exec = steam"));
        assert!(!lines.iter().any(|l| l.starts_with("--")));
    }

    #[test]
    fn flags_hidden_and_non_lua_program_lines() {
        let b = Bundle {
            name: "x".into(),
            entries: vec![
                Entry::File("hypr/hyprland.lua".into(), "--[[x]] os.execute(\"id\")
".into()),
                Entry::File("hypr/hypridle.conf".into(), "on-timeout = id
".into()),
                Entry::File("ironbar/config.toml".into(), "show_if = \"id\"
".into()),
                Entry::File("anyrun/config.ron".into(), "plugins: [\"/tmp/x.so\"],
".into()),
            ],
        };
        assert_eq!(risky_lines(&b).len(), 4);
    }

    #[test]
    fn absent_path_with_space_roundtrips() {
        let b = Bundle { name: "x".into(), entries: vec![Entry::Absent("osmium/modes/my mode.mode".into())] };
        assert_eq!(decode(&encode(&b), true).unwrap(), b);
    }

    #[test]
    fn gist_links_become_raw() {
        assert_eq!(raw_url("https://gist.github.com/u/abc123"), "https://gist.githubusercontent.com/u/abc123/raw");
        assert_eq!(raw_url("https://example.org/t.osmium-theme"), "https://example.org/t.osmium-theme");
    }
}
