//! OsmiumOS settings panel. A thin GUI over the osmium CLI tools plus a small
//! Hyprland appearance file (~/.config/hypr/osmium-settings.lua).

use iced::widget::{button, column, container, row, scrollable, slider, text, text_input, toggler};
use iced::{Element, Length, Task, Theme};
use std::path::PathBuf;
use std::process::Command;

fn main() -> iced::Result {
    if std::env::args().any(|a| a == "--sample-lua") {
        print!("{}", Appearance::default().to_lua());
        return Ok(());
    }
    iced::application(App::new, App::update, App::view)
        .title("OsmiumOS Ayarlar")
        .theme(|_: &App| Theme::Nord)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Appearance,
    Modes,
    History,
    Updates,
}

#[derive(Debug, Clone, PartialEq)]
struct Appearance {
    gaps_in: u8,
    gaps_out: u8,
    rounding: u8,
    blur: bool,
    animations: bool,
    kb_layout: String,
}

impl Default for Appearance {
    fn default() -> Self {
        // Same values as the shipped hyprland.lua.
        Appearance { gaps_in: 5, gaps_out: 10, rounding: 6, blur: true, animations: true, kb_layout: "tr".into() }
    }
}

impl Appearance {
    fn to_lua(&self) -> String {
        let layout: String = self.kb_layout.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ',').collect();
        format!(
            "-- Written by osmium-settings. Loaded at the end of hyprland.lua.\n\
             hl.config({{\n\
             \x20   general = {{ gaps_in = {}, gaps_out = {} }},\n\
             \x20   decoration = {{ rounding = {}, blur = {{ enabled = {} }} }},\n\
             \x20   animations = {{ enabled = {} }},\n\
             \x20   input = {{ kb_layout = \"{layout}\" }},\n\
             }})\n",
            self.gaps_in, self.gaps_out, self.rounding, self.blur, self.animations
        )
    }

    /// Reads values back from a file this app wrote; anything missing keeps its default.
    fn from_lua(src: &str) -> Self {
        let mut a = Appearance::default();
        let num = |key: &str| -> Option<u8> {
            let rest = &src[src.find(&format!("{key} = "))? + key.len() + 3..];
            rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
        };
        let flag = |section: &str| -> Option<bool> {
            let rest = &src[src.find(section)?..];
            let rest = &rest[rest.find("enabled = ")? + 10..];
            Some(rest.starts_with("true"))
        };
        a.gaps_in = num("gaps_in").unwrap_or(a.gaps_in);
        a.gaps_out = num("gaps_out").unwrap_or(a.gaps_out);
        a.rounding = num("rounding").unwrap_or(a.rounding);
        a.blur = flag("blur").unwrap_or(a.blur);
        a.animations = flag("animations").unwrap_or(a.animations);
        a.kb_layout = kb_layout_in(src).unwrap_or(a.kb_layout);
        a
    }

    /// What the panel shows before it has saved anything: the defaults, with the keyboard
    /// layout hyprland.lua already uses (the installer writes the chosen one there), so the
    /// first "save" does not switch the keyboard back to Turkish.
    fn initial(hyprland_lua: &str) -> Self {
        let mut a = Appearance::default();
        a.kb_layout = kb_layout_in(hyprland_lua).unwrap_or(a.kb_layout);
        a
    }
}

/// The first `kb_layout = "..."` outside a Lua comment.
fn kb_layout_in(src: &str) -> Option<String> {
    src.lines().filter(|l| !l.trim_start().starts_with("--")).find_map(|l| {
        let rest = l[l.find("kb_layout")? + 9..].trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
        Some(rest[..rest.find('"')?].to_string())
    })
}

#[derive(Debug, Clone, PartialEq)]
struct ModeRow {
    id: String,
    name: String,
    active: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    id: String,
    date: String,
    message: String,
}

struct App {
    tab: Tab,
    look: Appearance,
    modes: Vec<ModeRow>,
    history: Vec<Snapshot>,
    update_report: String,
    status: String,
}

#[derive(Debug, Clone)]
enum Msg {
    Tab(Tab),
    GapsIn(u8),
    GapsOut(u8),
    Rounding(u8),
    Blur(bool),
    Animations(bool),
    Layout(String),
    SaveLook,
    SetMode(String),
    Restore(String),
    CheckUpdates,
    UpdateReport(String),
    RunUpdate,
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

fn settings_file() -> PathBuf {
    config_dir().join("hypr/osmium-settings.lua")
}

/// Runs a tool; returns stdout, or a Turkish error line.
fn tool(cmd: &str, args: &[&str]) -> std::result::Result<String, String> {
    match Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() || o.status.code() == Some(2) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!("{cmd}: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(format!("{cmd} çalıştırılamadı: {e}")),
    }
}

fn parse_modes(out: &str) -> Vec<ModeRow> {
    out.lines()
        .filter_map(|l| {
            let active = l.starts_with('*');
            let mut parts = l.get(2..)?.split_whitespace();
            let id = parts.next()?.to_string();
            let name = parts.collect::<Vec<_>>().join(" ");
            Some(ModeRow { id, name, active })
        })
        .collect()
}

/// Snapshot lines from `osmium-history log` look like `abc1234  2026-11-06 09:10  message`.
fn parse_history(out: &str) -> Vec<Snapshot> {
    out.lines()
        .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_hexdigit()))
        .filter_map(|l| {
            let (id, rest) = l.split_once("  ")?;
            let (date, message) = rest.split_once("  ").unwrap_or((rest, ""));
            Some(Snapshot { id: id.into(), date: date.into(), message: message.into() })
        })
        .collect()
}

impl App {
    fn new() -> (Self, Task<Msg>) {
        let look = match std::fs::read_to_string(settings_file()) {
            Ok(s) => Appearance::from_lua(&s),
            Err(_) => Appearance::initial(&std::fs::read_to_string(config_dir().join("hypr/hyprland.lua")).unwrap_or_default()),
        };
        let mut app = App {
            tab: Tab::Appearance,
            look,
            modes: Vec::new(),
            history: Vec::new(),
            update_report: String::new(),
            status: String::new(),
        };
        app.refresh();
        (app, Task::none())
    }

    fn refresh(&mut self) {
        match tool("osmium-mode", &["list"]) {
            Ok(out) => self.modes = parse_modes(&out),
            Err(e) => self.status = e,
        }
        match tool("osmium-history", &["log"]) {
            Ok(out) => self.history = parse_history(&out),
            Err(e) => self.status = e,
        }
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Tab(t) => self.tab = t,
            Msg::GapsIn(v) => self.look.gaps_in = v,
            Msg::GapsOut(v) => self.look.gaps_out = v,
            Msg::Rounding(v) => self.look.rounding = v,
            Msg::Blur(v) => self.look.blur = v,
            Msg::Animations(v) => self.look.animations = v,
            Msg::Layout(v) => self.look.kb_layout = v,
            Msg::SaveLook => {
                let path = settings_file();
                let res = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&path, self.look.to_lua()));
                self.status = match res {
                    Ok(()) => match tool("hyprctl", &["reload"]) {
                        Ok(_) => "Kaydedildi ve uygulandı.".into(),
                        Err(e) => format!("Kaydedildi, ama yeniden yüklenemedi: {e}"),
                    },
                    Err(e) => format!("{}: {e}", path.display()),
                };
            }
            Msg::SetMode(id) => {
                self.status = match tool("osmium-mode", &["set", &id]) {
                    Ok(_) => format!("Mod: {id}"),
                    Err(e) => e,
                };
                self.refresh();
            }
            Msg::Restore(id) => {
                self.status = match tool("osmium-history", &["restore", &id]) {
                    Ok(out) => out.trim().to_string(),
                    Err(e) => e,
                };
                let _ = tool("hyprctl", &["reload"]);
                self.refresh();
            }
            Msg::CheckUpdates => {
                self.update_report = "Kontrol ediliyor...".into();
                // Network-bound: run off the UI thread.
                return Task::perform(
                    async {
                        let (tx, rx) = iced::futures::channel::oneshot::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(tool("osmium-update", &["check"]).unwrap_or_else(|e| e));
                        });
                        rx.await.unwrap_or_default()
                    },
                    Msg::UpdateReport,
                );
            }
            Msg::UpdateReport(r) => self.update_report = r,
            Msg::RunUpdate => {
                // pacman needs a terminal for sudo and prompts.
                self.status = match Command::new("alacritty").args(["-e", "osmium-update"]).spawn() {
                    Ok(_) => "Güncelleme terminalde açıldı.".into(),
                    Err(e) => format!("Terminal açılamadı: {e}"),
                };
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Msg> {
        let tab = |label: &'static str, t: Tab| {
            let b = button(text(label)).width(Length::Fill);
            if self.tab == t { b } else { b.on_press(Msg::Tab(t)).style(button::secondary) }
        };
        let sidebar = column![
            tab("Görünüm", Tab::Appearance),
            tab("Modlar", Tab::Modes),
            tab("Ayar geçmişi", Tab::History),
            tab("Güncellemeler", Tab::Updates),
        ]
        .spacing(6)
        .width(180);

        let body: Element<'_, Msg> = match self.tab {
            Tab::Appearance => {
                let l = &self.look;
                column![
                    text("Görünüm").size(24),
                    text(format!("İç boşluk: {}", l.gaps_in)),
                    slider(0..=30, l.gaps_in, Msg::GapsIn),
                    text(format!("Dış boşluk: {}", l.gaps_out)),
                    slider(0..=40, l.gaps_out, Msg::GapsOut),
                    text(format!("Köşe yuvarlaklığı: {}", l.rounding)),
                    slider(0..=30, l.rounding, Msg::Rounding),
                    toggler(l.blur).label("Bulanıklık").on_toggle(Msg::Blur),
                    toggler(l.animations).label("Animasyonlar").on_toggle(Msg::Animations),
                    text("Klavye düzeni (örn. tr veya tr,us)"),
                    text_input("tr", &l.kb_layout).on_input(Msg::Layout),
                    button(text("Kaydet ve uygula")).on_press(Msg::SaveLook),
                ]
                .spacing(10)
                .into()
            }
            Tab::Modes => {
                let mut c = column![text("Modlar").size(24), text("Tek tıkla masaüstü modu değiştir (Super+M).")].spacing(10);
                for m in &self.modes {
                    let label = if m.active { format!("{} (etkin)", m.name) } else { m.name.clone() };
                    c = c.push(button(text(label)).width(Length::Fill).on_press(Msg::SetMode(m.id.clone())));
                }
                c.into()
            }
            Tab::History => {
                let mut c = column![
                    text("Ayar geçmişi").size(24),
                    text("~/.config her 10 dakikada kaydedilir. Geri yükleme de geri alınabilir.")
                ]
                .spacing(8);
                for s in &self.history {
                    c = c.push(
                        row![
                            text(format!("{}  {}  {}", s.date, s.id, s.message)).width(Length::Fill),
                            button(text("Geri yükle")).on_press(Msg::Restore(s.id.clone())).style(button::secondary),
                        ]
                        .spacing(8),
                    );
                }
                scrollable(c).into()
            }
            Tab::Updates => column![
                text("Güncellemeler").size(24),
                row![
                    button(text("Riskleri kontrol et")).on_press(Msg::CheckUpdates),
                    button(text("Güncelle (terminal)")).on_press(Msg::RunUpdate).style(button::secondary),
                ]
                .spacing(8),
                scrollable(text(&self.update_report).font(iced::Font::MONOSPACE)),
            ]
            .spacing(10)
            .into(),
        };

        container(column![row![sidebar, container(body).width(Length::Fill)].spacing(20), text(&self.status)].spacing(12))
            .padding(20)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_roundtrip() {
        let a = Appearance { gaps_in: 0, gaps_out: 25, rounding: 3, blur: false, animations: true, kb_layout: "tr,us".into() };
        assert_eq!(Appearance::from_lua(&a.to_lua()), a);
    }

    #[test]
    fn first_run_keeps_the_installed_keyboard_layout() {
        let lua = "-- input = { kb_layout = \"xx\" }\nhl.config({\n    input = {\n        kb_layout    = \"us\",\n    },\n})\n";
        assert_eq!(Appearance::initial(lua), Appearance { kb_layout: "us".into(), ..Default::default() });
        assert_eq!(Appearance::initial(""), Appearance::default());
        assert_eq!(Appearance::initial("kb_layout = layout\n"), Appearance::default());
    }

    #[test]
    fn layout_is_sanitised() {
        let a = Appearance { kb_layout: "tr\") os.execute(\"x".into(), ..Default::default() };
        assert!(a.to_lua().contains("kb_layout = \"trosexecutex\""));
    }

    #[test]
    fn sample_file_matches_defaults() {
        let sample = include_str!("../sample-settings.lua");
        assert_eq!(Appearance::default().to_lua(), sample);
    }

    #[test]
    fn parses_tool_output() {
        let m = parse_modes("* normal       Normal\n  oyun         Oyun modu\n");
        assert_eq!(m[0], ModeRow { id: "normal".into(), name: "Normal".into(), active: true });
        assert_eq!(m[1].name, "Oyun modu");
        let h = parse_history("abc1234  2026-11-06 09:10  auto\n hypr/hyprland.lua | 2 +-\n 1 file changed\n");
        assert_eq!(h, vec![Snapshot { id: "abc1234".into(), date: "2026-11-06 09:10".into(), message: "auto".into() }]);
    }
}
