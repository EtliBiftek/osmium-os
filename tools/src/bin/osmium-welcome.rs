//! First-login welcome screen: synthwave logo plus the shortcuts a new user needs.
//!
//!   osmium-welcome            show it in this terminal
//!   osmium-welcome --launch   once per user: open it in a floating alacritty (Hyprland autostart)

use osmium_tools::{Result, exit_on_err, state_home};
use std::fs;
use std::io::{BufRead, Write};
use std::process::Command;

const LOGO: [&str; 5] = [
    " ██████  ███████ ███    ███ ██ ██    ██ ███    ███",
    "██    ██ ██      ████  ████ ██ ██    ██ ████  ████",
    "██    ██ ███████ ██ ████ ██ ██ ██    ██ ██ ████ ██",
    "██    ██      ██ ██  ██  ██ ██ ██    ██ ██  ██  ██",
    " ██████  ███████ ██      ██ ██  ██████  ██      ██",
];

/// Logo rows fade from neon pink to cyan (24-bit ANSI colours).
const ROW_COLOURS: [(u8, u8, u8); 5] = [(255, 43, 214), (214, 70, 230), (160, 110, 245), (90, 170, 250), (0, 229, 255)];

const SHORTCUTS: [(&str, &str); 9] = [
    ("Super + Enter", "Terminal"),
    ("Super + Space", "Uygulama menüsü"),
    ("Super + E", "Dosyalar"),
    ("Super + I", "Ayarlar"),
    ("Super + M", "Mod değiştir (Oyun, Çalışma, Sunum)"),
    ("Super + N", "Bildirimler"),
    ("Super + Q", "Pencereyi kapat"),
    ("Super + 1-9", "Çalışma alanları"),
    ("Print", "Ekran görüntüsü (seçerek, panoya)"),
];

fn main() {
    let res = match std::env::args().nth(1).as_deref() {
        None => show(),
        Some("--launch") => launch(),
        Some(_) => Err("usage: osmium-welcome [--launch]".into()),
    };
    exit_on_err("osmium-welcome", res);
}

fn render() -> String {
    let mut s = String::from("\n");
    for (row, (r, g, b)) in LOGO.iter().zip(ROW_COLOURS) {
        s += &format!("  \x1b[1;38;2;{r};{g};{b}m{row}\x1b[0m\n");
    }
    s += "\n  \x1b[38;2;255;176;0mOsmiumOS'a hoş geldin.\x1b[0m Kısayollar:\n\n";
    for (keys, what) in SHORTCUTS {
        s += &format!("  \x1b[38;2;0;229;255m{keys:<14}\x1b[0m {what}\n");
    }
    s += "\n  Ayarlar geçmişi: osmium-history   Güncelleme: osmium-update\n";
    s += "  Windows'tan taşı: osmium-migrate   Asistan: osmium-ai\n";
    s += "\n  \x1b[2mKapatmak için Enter'a bas.\x1b[0m ";
    s
}

fn show() -> Result {
    print!("{}", render());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    Ok(())
}

fn launch() -> Result {
    let marker = state_home()?.join("osmium/welcomed");
    if marker.exists() {
        return Ok(());
    }
    fs::create_dir_all(marker.parent().unwrap()).map_err(|e| format!("{}: {e}", marker.display()))?;
    // Mark first: a crash in the terminal must not reopen it on every login.
    fs::write(&marker, "").map_err(|e| format!("{}: {e}", marker.display()))?;
    Command::new("alacritty")
        .args([
            "--class", "osmium-welcome",
            "-o", "window.dimensions.columns=62",
            "-o", "window.dimensions.lines=24",
            "-e", "osmium-welcome",
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("alacritty: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_the_window() {
        // 62 columns, 24 lines (see launch()).
        let text = render();
        let plain: String = text.split('\x1b').enumerate().map(|(i, part)| {
            if i == 0 { part } else { part.split_once('m').map_or("", |(_, rest)| rest) }
        }).collect();
        assert!(plain.lines().count() <= 24, "{} lines", plain.lines().count());
        for l in plain.lines() {
            assert!(l.chars().count() <= 62, "too wide: {l}");
        }
    }
}
