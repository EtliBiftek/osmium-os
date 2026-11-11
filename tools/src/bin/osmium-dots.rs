//! Desktop "dots": the ready-made OsmiumOS desktops, or any dotfiles repo on GitHub.
//!
//!   osmium-dots list [--json]                          built-in dots (ii, waffle, m3ww, synthwave)
//!   osmium-dots search [--json] <words>                GitHub repos with topic "hyprland", most stars first
//!   osmium-dots apply <id|github:owner/repo> [--mode config|script]
//!   osmium-dots current                                what was applied last
//!   osmium-dots session                                login entry: default dots on first login, then hyprland-rust
//!   osmium-dots onizle <id> [--png <dosya>] [--sure <sn>]  try a dot in a window (nested hyprland-rust)
//!   osmium-dots parcalar <id>                          which parts of a dot can be mixed
//!   osmium-dots karistir <ad> <taban> <parça>=<id>...  a new dot: <taban> with parts of others
//!
//! Mixing. A dot's `info` may declare parts: `renk=#rrggbb` (its accent color), `duvar=<image>`
//! (its wallpaper), `cubuk=<paths under $HOME>` with `cubuk.baslat=<command>` (its bar). The base
//! says how it takes them: `renk.uygula=` / `duvar.uygula=` (commands with `{renk}` / `{duvar}`),
//! `cubuk.durdur=<process name>` (its own bar, stopped when another one is mixed in). A mix is a
//! user dot in ~/.local/share/osmium/dots/<ad>/ that inherits the base; its parts are applied by
//! a helper the session starts once the compositor runs (`karisim-baslat`).
//!
//! Built-in dots live in /usr/share/osmium/dots/<id>/: `info` (key=value), `home/` (mirrors
//! $HOME), optional `post-apply` (run as the user). `inherits=<id>` applies that one first.
//! Replaced ~/.config/<dir> entries are moved to ~/.local/state/osmium/dots-backup/<time>/.
//!
//! GitHub repos: `config` mode copies only their config dirs (nothing runs). `script` mode shows
//! the repo's install script and runs it after you type `çalıştır`; a script can do anything
//! your user (and sudo) can. Without a terminal (the installer) the script is postponed to the
//! first login, where a terminal asks you.

use osmium_tools::{Result, exit_on_err, json_str as js, output, state_home, succeeds};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DOTS_DIR: &str = "/usr/share/osmium/dots";
const DEFAULT: &str = "ii";
const SCRIPT_NAMES: &[&str] = &["install.sh", "setup.sh", "install", "setup", "scripts/install.sh"];
/// Where dotfiles repos keep their ~/.config tree, most specific first.
const CONFIG_ROOTS: &[&str] = &[".config", "dots/.config", "dotfiles/.config", ".dotfiles/.config", "home/.config", "config"];
/// Top-level repo entries that are never config dirs (repos that keep configs at the root).
const NOT_CONFIG: &[&str] = &[
    ".git", ".github", "assets", "screenshots", "images", "img", "preview", "previews", "docs", "wallpapers",
    "scripts", "LICENSE", "README.md",
];
const TERMINALS: &[&str] = &["kitty", "foot", "alacritty"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match a[..] {
        ["list"] => list(false),
        ["list", "--json"] => list(true),
        ["search", "--json", ref words @ ..] if !words.is_empty() => search(&words.join(" "), true),
        ["search", ref words @ ..] if !words.is_empty() => search(&words.join(" "), false),
        ["apply", target] => apply(target, Mode::Config),
        ["apply", target, "--mode", m] => Mode::parse(m).and_then(|m| apply(target, m)),
        ["current"] => {
            println!("{}", current().unwrap_or_default());
            Ok(())
        }
        ["session"] => session(),
        ["pending-run"] => pending_run(),
        ["onizle", id, ref rest @ ..] => preview(id, rest),
        ["parcalar", id] => parts(id),
        ["karistir", name, base, ref picks @ ..] if !picks.is_empty() => mix(name, base, picks),
        ["karisim-baslat"] => mix_start(),
        _ => Err("usage: osmium-dots list [--json] | search [--json] <words> | apply <id|github:owner/repo> [--mode config|script] | current | session | onizle <id> [--png <dosya>] [--sure <sn>] | parcalar <id> | karistir <ad> <taban> <parça>=<id>...".into()),
    };
    exit_on_err("osmium-dots", res);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Config,
    Script,
}

impl Mode {
    fn parse(s: &str) -> Result<Mode> {
        match s {
            "config" => Ok(Mode::Config),
            "script" => Ok(Mode::Script),
            _ => Err(format!("unknown mode `{s}` (config or script)")),
        }
    }
}

// ---------- built-in dots ----------

fn parse_info(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// The directory of dot `id`: the user's own (mixes) first, then the system's.
fn dot_dir(dir: &Path, id: &str) -> PathBuf {
    if dir == Path::new(DOTS_DIR)
        && let Ok(user) = user_dots_dir()
        && user.join(id).join("info").is_file()
    {
        return user;
    }
    dir.to_path_buf()
}

fn user_dots_dir() -> Result<PathBuf> {
    Ok(osmium_tools::data_home()?.join("osmium/dots"))
}

fn builtin(dir: &Path, id: &str) -> Result<BTreeMap<String, String>> {
    let p = dot_dir(dir, id).join(id).join("info");
    let text = fs::read_to_string(&p).map_err(|_| format!("no built-in dots named `{id}` (see: osmium-dots list)"))?;
    Ok(parse_info(&text))
}

/// `id` and its parents, parents first.
fn chain(dir: &Path, id: &str) -> Result<Vec<String>> {
    let mut ids = vec![id.to_string()];
    while let Some(parent) = builtin(dir, ids.last().unwrap())?.get("inherits").cloned() {
        if ids.contains(&parent) || ids.len() > 8 {
            return Err(format!("`{id}`: inherits loop"));
        }
        ids.push(parent);
    }
    ids.reverse();
    Ok(ids)
}

fn list(json: bool) -> Result {
    let dir = Path::new(DOTS_DIR);
    let mut ids: Vec<String> = fs::read_dir(dir)
        .map_err(|e| format!("{DOTS_DIR}: {e}"))?
        .chain(user_dots_dir().ok().and_then(|u| fs::read_dir(u).ok()).into_iter().flatten())
        .flatten()
        .filter(|e| e.path().join("info").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids.dedup();
    // Default first, the rest by name.
    ids.sort_by_key(|id| (id != DEFAULT, id.clone()));
    let mut items = Vec::new();
    for id in &ids {
        let info = builtin(dir, id)?;
        let get = |k: &str| info.get(k).cloned().unwrap_or_default();
        if json {
            // A shipped preview, else one `onizle --png` cached.
            let cached = home().map(|h| h.join(".cache/osmium/onizleme").join(format!("{id}.png"))).unwrap_or_default();
            let preview = Some(dot_dir(dir, id).join(id).join("preview.png")).filter(|p| p.is_file()).unwrap_or(cached);
            items.push(format!(
                "{{\"id\":{},\"title\":{},\"description\":{},\"source\":{},\"preview\":{}}}",
                js(id),
                js(&get("title")),
                js(&get("description")),
                js(&get("source")),
                js(&if preview.is_file() { preview.display().to_string() } else { String::new() }),
            ));
        } else {
            println!("{:<10} {}  {}", id, get("title"), get("description"));
        }
    }
    if json {
        println!("[{}]", items.join(","));
    }
    Ok(())
}

// ---------- apply ----------

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set".into())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn state_file(name: &str) -> Result<PathBuf> {
    Ok(state_home()?.join("osmium").join(name))
}

fn current() -> Option<String> {
    fs::read_to_string(state_file("dots").ok()?).ok().map(|s| s.trim().to_string())
}

fn set_current(target: &str) -> Result {
    let f = state_file("dots")?;
    fs::create_dir_all(f.parent().unwrap()).map_err(|e| format!("{}: {e}", f.parent().unwrap().display()))?;
    fs::write(&f, target).map_err(|e| format!("{}: {e}", f.display()))
}

fn apply(target: &str, mode: Mode) -> Result {
    match target.strip_prefix("github:") {
        Some(repo) => apply_github(repo, mode),
        None if mode == Mode::Script => Err("script mode is for GitHub repos only".into()),
        None => apply_builtin(Path::new(DOTS_DIR), target, &home()?),
    }
}

/// Names of the ~/.config subdirs/files a tree would replace.
fn config_entries(tree: &Path) -> Vec<String> {
    fs::read_dir(tree.join(".config"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// Moves the existing ~/.config/<name> entries out of the way, all into one backup dir.
fn backup(home: &Path, names: &[String]) -> Result<Option<PathBuf>> {
    let dest = home.join(".local/state/osmium/dots-backup").join(now().to_string());
    let mut moved = false;
    for name in names {
        let from = home.join(".config").join(name);
        if from.symlink_metadata().is_err() {
            continue;
        }
        fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        fs::rename(&from, dest.join(name)).map_err(|e| format!("{}: {e}", from.display()))?;
        moved = true;
    }
    Ok(moved.then_some(dest))
}

/// Copies `tree`'s contents over `dest` (merging dirs, replacing files).
fn copy_tree(tree: &Path, dest: &Path) -> Result {
    fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut src = tree.as_os_str().to_owned();
    src.push("/.");
    osmium_tools::run(Command::new("cp").arg("-a").arg("--no-preserve=ownership").arg(src).arg(dest))
}

fn apply_builtin(dir: &Path, id: &str, home: &Path) -> Result {
    let ids = chain(dir, id)?;
    let trees: Vec<PathBuf> = ids.iter().map(|i| dot_dir(dir, i).join(i).join("home")).collect();
    let mut names: Vec<String> = trees.iter().flat_map(|t| config_entries(t)).collect();
    names.sort();
    names.dedup();
    if let Some(b) = backup(home, &names)? {
        println!("Eski ayarlar yedeklendi: {}", b.display());
    }
    for t in &trees {
        if t.is_dir() {
            copy_tree(t, home)?;
        }
    }
    for i in &ids {
        let hook = dot_dir(dir, i).join(i).join("post-apply");
        if hook.is_file() && !succeeds(Command::new("sh").arg(&hook).env("HOME", home)) {
            eprintln!("osmium-dots: warning: {} failed", hook.display());
        }
    }
    set_current(id)?;
    // A new mix gets its parts on the next start of the helper.
    let _ = fs::remove_file(state_file("karisim-uygulandi")?);
    println!("Uygulandı: {id}. Oturumu kapatıp açınca tamamen geçer.");
    Ok(())
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

fn clone(repo: &str) -> Result<PathBuf> {
    if !valid_repo(repo) {
        return Err(format!("`{repo}` is not owner/repo"));
    }
    let dir = home()?.join(".cache/osmium/dots").join(repo.replace('/', "__"));
    osmium_tools::remove_dir(&dir)?;
    println!("İndiriliyor: https://github.com/{repo}");
    osmium_tools::run(
        Command::new("git")
            .args(["clone", "-q", "--depth", "1", "--recurse-submodules", "--shallow-submodules"])
            .arg(format!("https://github.com/{repo}.git"))
            .arg(&dir),
    )?;
    Ok(dir)
}

/// The dir whose children go into ~/.config.
fn find_config_root(repo: &Path) -> Option<PathBuf> {
    CONFIG_ROOTS.iter().map(|r| repo.join(r)).find(|p| p.join("hypr").is_dir())
        .or_else(|| CONFIG_ROOTS.iter().map(|r| repo.join(r)).find(|p| p.is_dir()))
        .or_else(|| repo.join("hypr").is_dir().then(|| repo.to_path_buf()))
}

fn apply_github(repo: &str, mode: Mode) -> Result {
    if mode == Mode::Script && !std::io::stdin().is_terminal() {
        // Installer / no terminal: give the user a working desktop now, ask at first login.
        if !valid_repo(repo) {
            return Err(format!("`{repo}` is not owner/repo"));
        }
        let p = state_file("dots-pending")?;
        fs::create_dir_all(p.parent().unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
        fs::write(&p, format!("github:{repo}")).map_err(|e| format!("{}: {e}", p.display()))?;
        println!("{repo} kurulum betiği ilk girişte, bir terminalde onayına sunulacak.");
        return apply_builtin(Path::new(DOTS_DIR), DEFAULT, &home()?);
    }
    let dir = clone(repo)?;
    match mode {
        Mode::Config => apply_config_dir(repo, &dir),
        Mode::Script => run_script(repo, &dir),
    }
}

fn apply_config_dir(repo: &str, dir: &Path) -> Result {
    let root = find_config_root(dir).ok_or_else(|| format!("{repo}: no .config or hypr directory found; try --mode script"))?;
    let names: Vec<String> = fs::read_dir(&root)
        .map_err(|e| format!("{}: {e}", root.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !NOT_CONFIG.contains(&n.as_str()) && !n.starts_with('.'))
        .collect();
    if names.is_empty() {
        return Err(format!("{repo}: empty config directory"));
    }
    let home = home()?;
    if let Some(b) = backup(&home, &names)? {
        println!("Eski ayarlar yedeklendi: {}", b.display());
    }
    // A fresh account (the installer) has no ~/.config yet, and cp does not create parents.
    let config = home.join(".config");
    fs::create_dir_all(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    for n in &names {
        let (from, to) = (root.join(n), home.join(".config").join(n));
        osmium_tools::run(Command::new("cp").arg("-a").arg("--no-preserve=ownership").arg(&from).arg(&to))?;
    }
    // Repos with a home-like layout also carry ~/.local/share (fonts, icons, scripts).
    if let Some(share) = root.parent().map(|p| p.join(".local/share")).filter(|p| p.is_dir()) {
        copy_tree(&share, &home.join(".local/share"))?;
    }
    println!("Kopyalandı: {}", names.join(", "));
    report_missing(&home.join(".config/hypr"))?;
    set_current(&format!("github:{repo}"))?;
    println!("Uygulandı: {repo}. Oturumu kapatıp açınca tamamen geçer.");
    Ok(())
}

/// Programs a Hyprland config starts: `exec(-once) = cmd`, `hl.exec_cmd("cmd")`.
fn started_programs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') || l.starts_with("--") {
            continue;
        }
        let cmd = if let Some(i) = l.find("exec_cmd(") {
            let rest = &l[i + 9..];
            let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
            q.and_then(|q| rest[1..].split(q).next())
        } else {
            l.split_once('=')
                .filter(|(k, _)| matches!(k.trim(), "exec" | "exec-once" | "exec-shutdown"))
                .map(|(_, v)| v)
        };
        let Some(cmd) = cmd else { continue };
        // `[workspace 1 silent] kitty`: drop the rule block.
        let cmd = match cmd.trim_start().strip_prefix('[') {
            Some(r) => r.split_once(']').map_or("", |(_, c)| c),
            None => cmd,
        };
        // Each `a && b; c & d | e` part starts a program.
        for part in cmd.split(['&', ';', '|']) {
            let word = part
                .split_whitespace()
                .find(|w| !w.contains('=') && !matches!(*w, "sleep" | "env" | "nohup" | "setsid" | "exec" | "uwsm" | "app2unit" | "--"))
                .unwrap_or("")
                .trim_matches(|c| c == '"' || c == '\'');
            if !word.is_empty()
                && !word.contains('/')
                && !word.starts_with('$')
                && !word.starts_with('~')
                && !word.starts_with('-')
                && !word.chars().next().unwrap().is_ascii_digit()
                && !out.iter().any(|w| w == word)
            {
                out.push(word.to_string());
            }
        }
    }
    out
}

fn in_path(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

fn config_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            config_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "conf" || x == "lua") {
            out.push(p);
        }
    }
}

fn report_missing(hypr: &Path) -> Result {
    let mut files = Vec::new();
    config_files(hypr, &mut files);
    let mut missing: Vec<String> = Vec::new();
    for f in files {
        for w in started_programs(&fs::read_to_string(&f).unwrap_or_default()) {
            if !in_path(&w) && !missing.contains(&w) {
                missing.push(w);
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // pacman -F needs the files database (sudo pacman -Fy once); without it we only name programs.
    let mut pkgs: Vec<String> = Vec::new();
    for m in &missing {
        if let Ok(o) = output(Command::new("pacman").args(["-Fq", &format!("/usr/bin/{m}")]))
            && let Some(p) = o.lines().next().and_then(|l| l.rsplit('/').next())
            && !pkgs.iter().any(|x| x == p)
        {
            pkgs.push(p.to_string());
        }
    }
    println!("Bu ayarların başlattığı ama kurulu olmayan programlar: {}", missing.join(", "));
    if pkgs.is_empty() {
        println!("Paketlerini bulmak için: sudo pacman -Fy, sonra osmium-dots apply'ı tekrar çalıştır.");
        return Ok(());
    }
    println!("Arch paketleri: {}", pkgs.join(" "));
    if std::io::stdin().is_terminal() && confirm("Kurulsun mu? 'evet' yaz:") {
        osmium_tools::run(Command::new("sudo").args(["pacman", "-S", "--needed"]).args(&pkgs))?;
    } else {
        println!("Kurmak için: sudo pacman -S --needed {}", pkgs.join(" "));
    }
    Ok(())
}

fn confirm(question: &str) -> bool {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "evet" | "yes")
}

fn run_script(repo: &str, dir: &Path) -> Result {
    let script = SCRIPT_NAMES
        .iter()
        .map(|s| dir.join(s))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("{repo}: no install script ({}); try --mode config", SCRIPT_NAMES.join(", ")))?;
    let text = fs::read_to_string(&script).unwrap_or_default();
    println!("\n===== {} ({} satır) =====", script.display(), text.lines().count());
    for l in text.lines().take(80) {
        println!("{l}");
    }
    if text.lines().count() > 80 {
        println!("... (tamamı: less {})", script.display());
    }
    println!("\nUYARI: Bu betik senin kullanıcınla (ve isterse sudo ile) her şeyi yapabilir: paket kurar,");
    println!("dosya siler, sistemi değiştirir. Yalnızca güvendiğin repolar için çalıştır.");
    print!("Çalıştırmak için 'çalıştır' yaz: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    if line.trim() != "çalıştır" {
        println!("Vazgeçildi.");
        return Ok(());
    }
    osmium_tools::run(Command::new("bash").arg(&script).current_dir(dir))?;
    set_current(&format!("github:{repo}"))?;
    println!("Bitti: {repo}.");
    Ok(())
}

// ---------- session ----------

/// greetd / tty login: first login gets the default dots, then hyprland-rust takes over.
fn session() -> Result {
    let home = home()?;
    if current().is_none() && !home.join(".config/hypr").exists()
        && let Err(e) = apply_builtin(Path::new(DOTS_DIR), DEFAULT, &home)
    {
        eprintln!("osmium-dots: {e}");
    }
    // A mixed dot: the helper applies its parts once the compositor runs.
    if current().is_some_and(|c| !c.starts_with("github:") && !mix_parts(&builtin(Path::new(DOTS_DIR), &c).unwrap_or_default()).is_empty()) {
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let _ = Command::new(me).arg("karisim-baslat").spawn();
    }
    if state_file("dots-pending")?.is_file() {
        // Detached helper: waits for the compositor, then opens a terminal to ask.
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let _ = Command::new(me).arg("pending-run").spawn();
    }
    exec_compositor()
}

#[cfg(unix)]
fn exec_compositor() -> Result {
    use std::os::unix::process::CommandExt;
    // hyprland-rust does not export these; portals and ii's scripts read them.
    let err = Command::new("hyprland-rust")
        .env("XDG_CURRENT_DESKTOP", "Hyprland")
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_SESSION_DESKTOP", "Hyprland")
        .exec();
    Err(format!("hyprland-rust: {err}"))
}

#[cfg(not(unix))]
fn exec_compositor() -> Result {
    Err("session needs Linux".into())
}

fn pending_run() -> Result {
    let p = state_file("dots-pending")?;
    let target = fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let target = target.trim();
    if !target.strip_prefix("github:").is_some_and(valid_repo) {
        return Err(format!("{}: bad entry `{target}`", p.display()));
    }
    // Wait up to a minute for the compositor's IPC.
    for _ in 0..60 {
        if succeeds(Command::new("hyprctl").arg("version")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let term = TERMINALS.iter().find(|t| in_path(t)).ok_or("no terminal (kitty, foot, alacritty) installed")?;
    let cmd = format!("{term} -e osmium-dots apply {target} --mode script");
    let reply = output(Command::new("hyprctl").args(["dispatch", "exec", &cmd]))?;
    if reply.trim() == "ok" {
        osmium_tools::remove_file(&p)?;
        Ok(())
    } else {
        Err(format!("hyprctl dispatch exec: {}", reply.trim()))
    }
}

// ---------- preview ----------

/// Runs the dot in a nested hyprland-rust (winit backend: a window on this desktop) with a
/// throwaway HOME, so nothing of the user's own setup changes. `--png` waits and screenshots it.
fn preview(id: &str, rest: &[&str]) -> Result {
    let (mut png, mut secs) = (None, 15u64);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match (*a, it.next()) {
            ("--png", Some(p)) => png = Some(PathBuf::from(p)),
            ("--sure", Some(s)) => secs = s.parse().map_err(|_| format!("geçersiz süre `{s}`"))?,
            _ => return Err("kullanım: osmium-dots onizle <id> [--png <dosya>] [--sure <sn>]".into()),
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("önizleme bir Wayland masaüstü içinde açılır (WAYLAND_DISPLAY yok)".into());
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR yok")?);
    let tmp = runtime.join("osmium-onizle").join(format!("{}-{}", id.replace(['/', ':'], "_"), std::process::id()));
    let res = preview_in(id, &tmp, &runtime, png.as_deref(), secs);
    let _ = fs::remove_dir_all(&tmp);
    res
}

fn preview_in(id: &str, tmp: &Path, runtime: &Path, png: Option<&Path>, secs: u64) -> Result {
    let home = tmp.join("home");
    fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let with_home = |c: &mut Command| {
        c.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"));
    };
    // The user's mixes live in their own data dir: copy the one asked for (and its parts).
    if let Ok(user) = user_dots_dir()
        && user.join(id).join("info").is_file()
    {
        copy_tree(&user.join(id), &home.join(".local/share/osmium/dots").join(id))?;
    }
    let mut apply = Command::new(&me);
    with_home(&mut apply);
    osmium_tools::run(apply.args(["apply", id]))?;
    let mut comp = Command::new("dbus-run-session");
    with_home(&mut comp);
    comp.args(["--", "hyprland-rust", "--backend", "winit"])
        .env("XDG_CURRENT_DESKTOP", "Hyprland")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let started = SystemTime::now();
    let mut child = comp.spawn().map_err(|e| format!("hyprland-rust: {e}"))?;
    let Some(png) = png else {
        println!("Önizleme penceresi açıldı; kapatınca geçici dosyalar silinir.");
        let _ = child.wait();
        return Ok(());
    };
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let socket = nested_socket(runtime, started);
    let shot = match &socket {
        Some(s) => osmium_tools::run(Command::new("grim").env("WAYLAND_DISPLAY", s).arg(png)),
        None => Err("önizleme bileşimcisi başlamadı".into()),
    };
    let _ = child.kill();
    let _ = child.wait();
    shot?;
    println!("Önizleme: {}", png.display());
    Ok(())
}

/// The Wayland socket of the compositor instance started after `since`, from its
/// `hyprland.lock` (pid, then socket name).
fn nested_socket(runtime: &Path, since: SystemTime) -> Option<String> {
    let dirs = fs::read_dir(runtime.join("hypr")).ok()?.flatten();
    let newest = dirs
        .filter(|e| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    let lock = fs::read_to_string(newest.path().join("hyprland.lock")).ok()?;
    lock.lines().nth(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ---------- mixing ----------

const PARTS: &[&str] = &["renk", "duvar", "cubuk"];

/// The parts a dot's info offers: (part, value).
fn offered(info: &BTreeMap<String, String>) -> Vec<(&'static str, String)> {
    PARTS.iter().filter_map(|p| info.get(*p).filter(|v| !v.is_empty()).map(|v| (*p, v.clone()))).collect()
}

/// What a base accepts: each part needs its way to apply (or, for the bar, to stop its own).
fn accepts(info: &BTreeMap<String, String>, part: &str) -> bool {
    match part {
        "renk" | "duvar" => info.get(&format!("{part}.uygula")).is_some_and(|v| !v.is_empty()),
        "cubuk" => info.get("cubuk.durdur").is_some_and(|v| !v.is_empty()),
        _ => false,
    }
}

/// `karisim.<part>` entries of a mix.
fn mix_parts(info: &BTreeMap<String, String>) -> Vec<(String, String)> {
    info.iter().filter_map(|(k, v)| k.strip_prefix("karisim.").map(|p| (p.to_string(), v.clone()))).collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|h| h.is_ascii_hexdigit())
}

/// Every chain member's info merged, the dot itself last (so its keys win).
fn resolved_info(dir: &Path, id: &str) -> Result<BTreeMap<String, String>> {
    let mut all = BTreeMap::new();
    for i in chain(dir, id)? {
        all.extend(builtin(dir, &i)?);
    }
    Ok(all)
}

fn parts(id: &str) -> Result {
    let info = resolved_info(Path::new(DOTS_DIR), id)?;
    let offer = offered(&info);
    println!("{id} başkasına verebilir: {}", if offer.is_empty() { "-".into() } else { offer.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", ") });
    let take: Vec<&str> = PARTS.iter().copied().filter(|p| accepts(&info, p)).collect();
    println!("{id} başkasından alabilir: {}", if take.is_empty() { "-".into() } else { take.join(", ") });
    Ok(())
}

/// The info file of a mix, after checking that the base takes each part and the source has it.
fn mix_info(name: &str, base: &BTreeMap<String, String>, base_id: &str, picks: &[(String, String, String)]) -> Result<String> {
    let mut text = format!("name={name}\ntitle={name} ({base_id} karışımı)\ninherits={base_id}\n");
    let mut desc = Vec::new();
    for (part, from, value) in picks {
        if !accepts(base, part) {
            return Err(format!("`{base_id}` {part} parçasını başkasından alamıyor (bkz: osmium-dots parcalar {base_id})"));
        }
        if value.contains('\n') || (part == "renk" && !valid_color(value)) {
            return Err(format!("`{from}`: geçersiz {part} değeri"));
        }
        text += &format!("karisim.{part}={value}\n");
        if part == "cubuk" {
            text += &format!("karisim.cubuk.kaynak={from}\n");
        }
        desc.push(format!("{part}: {from}"));
    }
    text += &format!("description={base_id} tabanı; {}\nlicense=GPL-3.0-or-later\n", desc.join(", "));
    Ok(text)
}

fn mix(name: &str, base_id: &str, picks: &[&str]) -> Result {
    if !valid_name(name) {
        return Err("karışım adı küçük harf, rakam ve - olmalı (en çok 40 karakter)".into());
    }
    let dir = Path::new(DOTS_DIR);
    if dir.join(name).join("info").is_file() {
        return Err(format!("`{name}` hazır bir dotun adı; başka bir ad seçin"));
    }
    let base = resolved_info(dir, base_id)?;
    let mut resolved = Vec::new();
    let mut bar_files = None;
    for pick in picks {
        let (part, from) = pick.split_once('=').ok_or_else(|| format!("`{pick}`: <parça>=<dot> bekleniyordu"))?;
        if !PARTS.contains(&part) {
            return Err(format!("bilinmeyen parça `{part}` (renk, duvar, cubuk)"));
        }
        let src = resolved_info(dir, from)?;
        let value = src.get(part).filter(|v| !v.is_empty()).ok_or_else(|| format!("`{from}` {part} parçası vermiyor"))?.clone();
        if part == "cubuk" {
            let start = src.get("cubuk.baslat").filter(|v| !v.is_empty()).ok_or_else(|| format!("`{from}`: cubuk.baslat yok"))?;
            bar_files = Some((from.to_string(), value.clone()));
            resolved.push(("cubuk".to_string(), from.to_string(), start.clone()));
        } else {
            resolved.push((part.to_string(), from.to_string(), value));
        }
    }
    let info = mix_info(name, &base, base_id, &resolved)?;
    let out = user_dots_dir()?.join(name);
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(out.join("home")).map_err(|e| format!("{}: {e}", out.display()))?;
    // The bar's files come along; everything else is the base's.
    if let Some((from, paths)) = bar_files {
        for rel in paths.split_whitespace() {
            if rel.starts_with('/') || rel.contains("..") {
                return Err(format!("`{from}`: geçersiz yol `{rel}`"));
            }
            let src = chain(dir, &from)?.iter().rev().map(|i| dot_dir(dir, i).join(i).join("home").join(rel)).find(|p| p.exists());
            let src = src.ok_or_else(|| format!("`{from}`: {rel} bulunamadı"))?;
            let dest = out.join("home").join(rel);
            fs::create_dir_all(dest.parent().unwrap()).map_err(|e| format!("{}: {e}", dest.display()))?;
            osmium_tools::run(Command::new("cp").arg("-a").arg("--no-preserve=ownership").arg(&src).arg(&dest))?;
        }
    }
    fs::write(out.join("info"), info).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("Karışım hazır: {name}. Denemek için: osmium-dots onizle {name}   Uygulamak için: osmium-dots apply {name}");
    Ok(())
}

/// A base's apply command with the part's value filled in, run through `sh -c` with the
/// value as "$1" so it is never parsed as shell text.
fn apply_command(template: &str, part: &str) -> String {
    template.replace(&format!("{{{part}}}"), "\"$1\"")
}

/// After login: give the compositor the mixed-in parts. Colors and wallpaper once per mix,
/// the bar on every login (the base's own is stopped first).
fn mix_start() -> Result {
    let Some(id) = current() else { return Ok(()) };
    let info = resolved_info(Path::new(DOTS_DIR), &id)?;
    let parts = mix_parts(&info);
    for _ in 0..60 {
        if succeeds(Command::new("hyprctl").arg("version")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    // Shells set up their color tools during the first seconds.
    std::thread::sleep(std::time::Duration::from_secs(5));
    let done = state_file("karisim-uygulandi")?;
    let first = fs::read_to_string(&done).map(|d| d.trim() != id).unwrap_or(true);
    for (part, value) in &parts {
        match part.as_str() {
            "renk" | "duvar" if first => {
                if let Some(t) = info.get(&format!("{part}.uygula")) {
                    let ok = succeeds(Command::new("sh").args(["-c", &apply_command(t, part), "osmium-dots", value]));
                    if !ok {
                        eprintln!("osmium-dots: {part} uygulanamadı");
                    }
                }
            }
            "cubuk" => {
                if let Some(own) = info.get("cubuk.durdur") {
                    succeeds(Command::new("pkill").args(["-x", own]));
                }
                let _ = output(Command::new("hyprctl").args(["dispatch", "exec", value]));
            }
            _ => {}
        }
    }
    let _ = fs::write(&done, &id);
    Ok(())
}

// ---------- GitHub search ----------

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct Repo {
    name: String,
    description: String,
    stars: u64,
}

fn search(words: &str, json: bool) -> Result {
    let url = format!(
        "https://api.github.com/search/repositories?q={}+topic:hyprland&sort=stars&order=desc&per_page=30",
        url_encode(words)
    );
    let body = output(Command::new("curl").args(["-fsSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", &url]))?;
    let repos = parse_search(&body)?;
    if json {
        let items: Vec<String> = repos
            .iter()
            .map(|r| format!("{{\"repo\":{},\"description\":{},\"stars\":{}}}", js(&r.name), js(&r.description), r.stars))
            .collect();
        println!("[{}]", items.join(","));
    } else {
        for r in &repos {
            println!("★{:<6} {:<40} {}", r.stars, r.name, r.description);
        }
        println!("\nUygulamak için: osmium-dots apply github:<sahip/repo> [--mode config|script]");
    }
    Ok(())
}

fn parse_search(body: &str) -> Result<Vec<Repo>> {
    let v = Json::parse(body)?;
    let items = match v.get("items") {
        Some(Json::Arr(a)) => a,
        _ => return Err(format!("GitHub: {}", v.get("message").and_then(Json::as_str).unwrap_or("unexpected reply"))),
    };
    Ok(items
        .iter()
        .filter_map(|it| {
            Some(Repo {
                name: it.get("full_name")?.as_str()?.to_string(),
                description: it.get("description").and_then(Json::as_str).unwrap_or("").to_string(),
                stars: match it.get("stargazers_count") {
                    Some(Json::Num(n)) => *n as u64,
                    _ => 0,
                },
            })
        })
        .collect())
}

/// Just enough JSON for GitHub's search reply.
#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(s: &str) -> Result<Json> {
        let mut p = Parser { s: s.as_bytes(), i: 0 };
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err("JSON: trailing data".into());
        }
        Ok(v)
    }
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, lit: &str) -> Result {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(format!("JSON: expected `{lit}` at {}", self.i))
        }
    }
    fn value(&mut self) -> Result<Json> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.eat(":")?;
                    kv.push((k, self.value()?));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(kv));
                        }
                        _ => return Err(format!("JSON: bad object at {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return Err(format!("JSON: bad array at {}", self.i)),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.eat("true").map(|_| Json::Bool(true)),
            Some(b'f') => self.eat("false").map(|_| Json::Bool(false)),
            Some(b'n') => self.eat("null").map(|_| Json::Null),
            Some(_) => {
                let start = self.i;
                while self.i < self.s.len() && b"+-.eE0123456789".contains(&self.s[self.i]) {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()
                    .and_then(|n| n.parse().ok())
                    .map(Json::Num)
                    .ok_or_else(|| format!("JSON: bad value at {start}"))
            }
            None => Err("JSON: unexpected end".into()),
        }
    }
    fn string(&mut self) -> Result<String> {
        self.eat("\"")?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or("JSON: unterminated string")?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|e| e.to_string()),
                b'\\' => {
                    let e = *self.s.get(self.i).ok_or("JSON: bad escape")?;
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' | b'f' => {}
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4).ok_or("JSON: bad \\u")?)
                                .map_err(|e| e.to_string())?;
                            self.i += 4;
                            let mut cp = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                            // Surrogate pair (emoji in descriptions).
                            if (0xD800..0xDC00).contains(&cp) && self.s[self.i..].starts_with(b"\\u") {
                                let lo = self.s.get(self.i + 2..self.i + 6).ok_or("JSON: bad \\u")?;
                                let lo = std::str::from_utf8(lo).map_err(|e| e.to_string())?;
                                let lo = u32::from_str_radix(lo, 16).map_err(|e| e.to_string())?;
                                self.i += 6;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            out.extend_from_slice(ch.to_string().as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(text: &str) -> BTreeMap<String, String> {
        parse_info(text)
    }

    #[test]
    fn parts_offered_and_accepted() {
        let synth = info("name=synthwave\nrenk=#ff2a6d\nduvar=/usr/share/backgrounds/osmium/synthwave.png\ncubuk=.config/ironbar\ncubuk.baslat=ironbar\ncubuk.durdur=ironbar\nduvar.uygula=awww img {duvar}\n");
        let ii = info("name=ii\nrenk.uygula=~/.config/quickshell/ii/scripts/colors/switchwall.sh --color {renk}\nduvar.uygula=~/.config/quickshell/ii/scripts/colors/switchwall.sh --image {duvar}\n");
        assert_eq!(offered(&synth).iter().map(|(p, _)| *p).collect::<Vec<_>>(), ["renk", "duvar", "cubuk"]);
        assert!(offered(&ii).is_empty());
        assert!(accepts(&ii, "renk") && accepts(&ii, "duvar") && !accepts(&ii, "cubuk"));
        assert!(accepts(&synth, "cubuk") && accepts(&synth, "duvar") && !accepts(&synth, "renk"));
    }

    #[test]
    fn mix_info_checks_the_base() {
        let ii = info("renk.uygula=x --color {renk}\nduvar.uygula=x --image {duvar}\n");
        let picks = vec![("renk".to_string(), "synthwave".to_string(), "#ff2a6d".to_string())];
        let text = mix_info("neon", &ii, "ii", &picks).unwrap();
        let parsed = info(&text);
        assert_eq!(parsed["inherits"], "ii");
        assert_eq!(parsed["karisim.renk"], "#ff2a6d");
        assert_eq!(mix_parts(&parsed), [("renk".to_string(), "#ff2a6d".to_string())]);
        let bar = vec![("cubuk".to_string(), "synthwave".to_string(), "ironbar".to_string())];
        assert!(mix_info("x", &ii, "ii", &bar).unwrap_err().contains("cubuk"));
        let bad = vec![("renk".to_string(), "s".to_string(), "pink; rm -rf ~".to_string())];
        assert!(mix_info("x", &ii, "ii", &bad).is_err());
        assert!(valid_name("neon-ii") && !valid_name("Neon") && !valid_name("../x") && !valid_name(""));
    }

    #[test]
    fn apply_command_passes_the_value_as_an_argument() {
        assert_eq!(apply_command("switchwall.sh --image {duvar}", "duvar"), "switchwall.sh --image \"$1\"");
        assert_eq!(apply_command("awww img {duvar} --x", "duvar"), "awww img \"$1\" --x");
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osmium-dots-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn info_and_inherits() {
        let d = tmp("chain");
        for (id, info) in [("ii", "name=ii\ntitle=I"), ("waffle", "# c\nname=waffle\ninherits=ii"), ("loop", "inherits=loop")] {
            fs::create_dir_all(d.join(id)).unwrap();
            fs::write(d.join(id).join("info"), info).unwrap();
        }
        assert_eq!(chain(&d, "waffle").unwrap(), ["ii", "waffle"]);
        assert_eq!(chain(&d, "ii").unwrap(), ["ii"]);
        assert!(chain(&d, "loop").is_err());
        assert!(chain(&d, "nope").unwrap_err().contains("osmium-dots list"));
        assert_eq!(parse_info("a = 1\nb=x=y\n#c=2").get("b").map(String::as_str), Some("x=y"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn backup_moves_only_existing() {
        let h = tmp("backup");
        fs::create_dir_all(h.join(".config/hypr")).unwrap();
        fs::write(h.join(".config/hypr/hyprland.lua"), "old").unwrap();
        let b = backup(&h, &["hypr".into(), "kitty".into()]).unwrap().unwrap();
        assert!(!h.join(".config/hypr").exists());
        assert_eq!(fs::read_to_string(b.join("hypr/hyprland.lua")).unwrap(), "old");
        assert!(backup(&h, &["kitty".into()]).unwrap().is_none());
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn repo_names() {
        assert!(valid_repo("end-4/dots-hyprland"));
        assert!(valid_repo("a_b/c.d"));
        for bad in ["end-4", "a/b/c", "../x", "a/..", "a b/c", "a/b;rm", ""] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn config_root_layouts() {
        let r = tmp("roots");
        fs::create_dir_all(r.join("dots/.config/hypr")).unwrap();
        fs::create_dir_all(r.join("config")).unwrap();
        assert_eq!(find_config_root(&r).unwrap(), r.join("dots/.config"));
        let flat = tmp("flat");
        fs::create_dir_all(flat.join("hypr")).unwrap();
        assert_eq!(find_config_root(&flat).unwrap(), flat);
        let none = tmp("none");
        assert!(find_config_root(&none).is_none());
        for d in [r, flat, none] {
            fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn programs_from_configs() {
        let conf = "exec-once = waybar & swaync\nexec = sleep 1 && hyprpaper\n# exec-once = commented\n\
                    exec-once = [workspace 1 silent] kitty\nexec-once = $HOME/x.sh\nexec-once = ~/y\n\
                    exec-once = env A=1 nm-applet --indicator\n";
        assert_eq!(started_programs(conf), ["waybar", "swaync", "hyprpaper", "kitty", "nm-applet"]);
        let lua = "hl.exec_cmd(\"qs -c ii\")\n-- hl.exec_cmd(\"no\")\n    hl.exec_cmd('/usr/bin/x')\nhl.exec_cmd(\"qs\")";
        assert_eq!(started_programs(lua), ["qs"]);
    }

    #[test]
    fn json_escape_and_parse() {
        assert_eq!(js("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        let v = Json::parse(r#"{"a":[1,-2.5e1,true,null],"s":"xç😀\"","o":{}}"#).unwrap();
        assert_eq!(v.get("s").and_then(Json::as_str), Some("xç😀\""));
        assert_eq!(v.get("a"), Some(&Json::Arr(vec![Json::Num(1.0), Json::Num(-25.0), Json::Bool(true), Json::Null])));
        assert!(Json::parse("{\"a\":1} x").is_err());
        assert!(Json::parse("[1,").is_err());
        // A reply cut inside a surrogate pair is an error, not a panic.
        assert!(Json::parse(r#"["\ud83d\ude"#).is_err());
    }

    #[test]
    fn github_search_reply() {
        let body = r#"{"total_count":2,"items":[
            {"full_name":"end-4/dots-hyprland","description":"ii","stargazers_count":16553},
            {"full_name":"x/y","description":null,"stargazers_count":3}]}"#;
        let r = parse_search(body).unwrap();
        assert_eq!(r[0], Repo { name: "end-4/dots-hyprland".into(), description: "ii".into(), stars: 16553 });
        assert_eq!(r[1].description, "");
        assert!(parse_search(r#"{"message":"API rate limit exceeded"}"#).unwrap_err().contains("rate limit"));
    }

    #[test]
    fn encodes_query() {
        assert_eq!(url_encode("material you/ç"), "material+you%2F%C3%A7");
    }
}
