//! Offline, local, Turkish-speaking system assistant (Ollama on 127.0.0.1).
//!
//!   osmium-ai                  interactive chat (`çıkış` to quit)
//!   osmium-ai ask "<soru>"     one question
//!   osmium-ai fix "<sorun>"    collect read-only system context, then ask
//!   osmium-ai setup            check ollama, pull the model after `evet`
//!
//! The model can only PROPOSE commands (```komut``` blocks). Each one is shown
//! with a risk label and runs only after you type `evet` (or `çalıştır` for
//! dangerous ones). Conversations live in memory only. Config:
//! ~/.config/osmium/ai.conf (`model`, `host`).

use osmium_tools::{Result, config_home, exit_on_err, output, run, succeeds};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

const DEFAULT_MODEL: &str = "qwen3:4b";
const DEFAULT_HOST: &str = "127.0.0.1:11434";
const CONTEXT_CAP: usize = 12 * 1024;
const OUTPUT_CAP: usize = 4 * 1024;
const HISTORY_MAX: usize = 24;
const SYSTEM_PROMPT: &str = "Sen OsmiumOS asistanısın. OsmiumOS, Arch Linux ve Hyprland 0.57 \
tabanlı bir dağıtımdır; Hyprland ayarı ~/.config/hypr/hyprland.lua (Lua) dosyasındadır. \
Şu osmium araçlarını bilirsin: osmium-mode (çalışma/oyun/sunum modları), osmium-history \
(sistem geçmişi), osmium-update (güncelleme), osmium-share (tema paylaşımı), osmium-migrate. \
Türkçe ve kısa cevap ver. Çalıştırılmasını istediğin her komutu ```komut ... ``` çitli \
bloğuna yaz; komutları sen çalıştırmazsın, kullanıcı onaylar. Emin değilsen tahmin etme, söyle.";

#[derive(Debug, PartialEq)]
struct Config {
    model: String,
    host: String,
}

fn parse_conf(text: &str) -> Config {
    let mut c = Config { model: DEFAULT_MODEL.into(), host: DEFAULT_HOST.into() };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "model" => c.model = v.into(),
            "host" => c.host = v.into(),
            _ => {}
        }
    }
    c
}

fn load_config() -> Result<Config> {
    let path = config_home()?.join("osmium/ai.conf");
    let c = parse_conf(&std::fs::read_to_string(path).unwrap_or_default());
    let name = c.host.rsplit_once(':').map_or(c.host.as_str(), |(h, _)| h);
    if !["127.0.0.1", "localhost", "[::1]"].contains(&name) {
        return Err(format!("host yalnızca yerel olabilir (127.0.0.1, localhost, [::1]): {}", c.host));
    }
    Ok(c)
}

// ---------- JSON (just enough for /api/chat) ----------

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn chat_body(model: &str, msgs: &[(String, String)]) -> String {
    let list: Vec<String> = msgs
        .iter()
        .map(|(r, c)| format!("{{\"role\":{},\"content\":{}}}", json_str(r), json_str(c)))
        .collect();
    format!(
        "{{\"model\":{},\"stream\":false,\"think\":false,\"messages\":[{}]}}",
        json_str(model),
        list.join(",")
    )
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}

fn hex4(b: &[u8], i: usize) -> Option<u32> {
    let s = std::str::from_utf8(b.get(i..i + 4)?).ok()?;
    u32::from_str_radix(s, 16).ok()
}

/// Parses a JSON string starting at the opening quote.
fn parse_string(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) != Some(&b'"') {
        return None;
    }
    *i += 1;
    let mut out = Vec::new();
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        match c {
            b'"' => return String::from_utf8(out).ok(),
            b'\\' => {
                let e = *b.get(*i)?;
                *i += 1;
                let ch = match e {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'"' | b'\\' | b'/' => e as char,
                    b'u' => {
                        let mut cp = hex4(b, *i)?;
                        *i += 4;
                        if (0xD800..0xDC00).contains(&cp) && b.get(*i..*i + 2) == Some(b"\\u") {
                            let lo = hex4(b, *i + 2)?;
                            *i += 6;
                            cp = 0x10000 + ((cp - 0xD800) << 10) + lo.wrapping_sub(0xDC00);
                        }
                        char::from_u32(cp).unwrap_or('\u{fffd}')
                    }
                    _ => return None,
                };
                let mut buf = [0; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            c => out.push(c),
        }
    }
}

fn skip_value(b: &[u8], i: &mut usize) -> Option<()> {
    match *b.get(*i)? {
        b'"' => parse_string(b, i).map(|_| ()),
        b'{' | b'[' => {
            let mut depth = 0;
            loop {
                match *b.get(*i)? {
                    b'"' => {
                        parse_string(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    _ => {}
                }
                *i += 1;
                if depth == 0 {
                    return Some(());
                }
            }
        }
        _ => {
            while b.get(*i).is_some_and(|c| !b",}] \t\r\n".contains(c)) {
                *i += 1;
            }
            Some(())
        }
    }
}

/// String value at `path` (object keys) in a JSON document.
fn json_get(json: &str, path: &[&str]) -> Option<String> {
    fn walk(b: &[u8], i: &mut usize, path: &[&str]) -> Option<String> {
        skip_ws(b, i);
        let Some((first, rest)) = path.split_first() else { return parse_string(b, i) };
        if b.get(*i) != Some(&b'{') {
            return None;
        }
        *i += 1;
        loop {
            skip_ws(b, i);
            match b.get(*i)? {
                b'}' => return None,
                b',' => {
                    *i += 1;
                    continue;
                }
                _ => {}
            }
            let key = parse_string(b, i)?;
            skip_ws(b, i);
            if b.get(*i) != Some(&b':') {
                return None;
            }
            *i += 1;
            skip_ws(b, i);
            if key == *first {
                return walk(b, i, rest);
            }
            skip_value(b, i)?;
        }
    }
    walk(json.as_bytes(), &mut 0, path)
}

fn parse_reply(resp: &str) -> Result<String> {
    if let Some(c) = json_get(resp, &["message", "content"]) {
        let c = c.trim();
        let c = match (c.starts_with("<think>"), c.find("</think>")) {
            (true, Some(e)) => &c[e + "</think>".len()..],
            _ => c,
        };
        return Ok(c.trim().to_string());
    }
    match json_get(resp, &["error"]) {
        Some(e) => Err(format!("ollama: {e}")),
        None => Err("ollama: anlaşılmayan cevap".into()),
    }
}

fn chat(cfg: &Config, msgs: &[(String, String)]) -> Result<String> {
    let body = chat_body(&cfg.model, msgs);
    let mut child = Command::new("curl")
        .args(["-sS", "--noproxy", "*", "--max-time", "600", "-H", "Content-Type: application/json"])
        .args(["--data-binary", "@-"])
        .arg(format!("http://{}/api/chat", cfg.host))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("curl stdin")?
        .write_all(body.as_bytes())
        .map_err(|e| format!("curl: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ollama'ya ulaşılamadı ({}): {}\n`osmium-ai setup` ile kontrol et.",
            cfg.host,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_reply(&String::from_utf8_lossy(&out.stdout))
}

// ---------- commands, risk, redaction ----------

/// Contents of every complete ```komut block; unterminated ones are dropped.
fn extract_commands(reply: &str) -> Vec<String> {
    reply
        .split("```komut")
        .skip(1)
        .filter_map(|part| {
            let (body, _) = part.split_once("```")?;
            let body = body.trim();
            (!body.is_empty()).then(|| body.to_string())
        })
        .collect()
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Risk {
    Normal,
    Elevated,
    Dangerous,
}

fn classify(cmd: &str) -> Risk {
    let spaced: String = cmd.chars().map(|c| if ";|&()`".contains(c) { ' ' } else { c }).collect();
    let t: Vec<&str> = spaced.split_whitespace().collect();
    let has = |w: &str| t.contains(&w);
    let flag = |c: char| t.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(c));
    let recursive = flag('R') || cmd.contains("--recursive");
    let compact: String = cmd.split_whitespace().collect();
    let pipe_to_shell = (has("curl") || has("wget"))
        && cmd.split('|').skip(1).any(|seg| {
            let s = seg.trim_start().trim_start_matches("sudo ").trim_start();
            ["sh", "bash", "zsh"].iter().any(|sh| s == *sh || s.starts_with(&format!("{sh} ")))
        });
    let dangerous = (has("rm") && (flag('r') || recursive))
        || has("dd")
        || t.iter().any(|a| a.starts_with("mkfs"))
        || has("wipefs")
        || (has("chmod") && recursive)
        || (has("chown") && recursive)
        || pipe_to_shell
        || compact.contains(">/dev/sd")
        || compact.contains(">/dev/nvme")
        || compact.contains(":(){")
        || (has("pacman") && t.iter().any(|a| a.starts_with("-R") || *a == "--remove"))
        || t.windows(2).any(|p| p[0] == "systemctl" && ["disable", "mask"].contains(&p[1]))
        || cmd.contains("/etc/fstab")
        || cmd.contains("/boot");
    if dangerous {
        Risk::Dangerous
    } else if has("sudo") || has("pkexec") || has("doas") {
        Risk::Elevated
    } else {
        Risk::Normal
    }
}

fn redact(text: &str) -> String {
    const WORDS: &[&str] = &["password", "passwd", "token", "secret", "psk", "key="];
    text.lines()
        .map(|l| {
            let low = l.to_lowercase();
            if !WORDS.iter().any(|w| low.contains(w)) {
                return l.to_string();
            }
            match l.find(['=', ':']) {
                Some(p) => format!("{} ***", &l[..=p]),
                None => "***".into(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keeps the last `max` bytes (errors are at the end), on a char boundary.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    const MARK: &str = "…\n";
    let mut start = s.len().saturating_sub(max.saturating_sub(MARK.len()));
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("{MARK}{}", &s[start..])
}

/// Joins sections under headers; every section gets an equal share of `max`.
fn build_context(sections: &[(&str, String)], max: usize) -> String {
    let per = max / sections.len().max(1);
    sections
        .iter()
        .map(|(h, body)| {
            let head = format!("## {h}\n");
            let room = per.saturating_sub(head.len() + 1);
            format!("{head}{}\n", cap(&redact(body), room))
        })
        .collect()
}

fn gather_context() -> String {
    let sh = |prog: &str, args: &[&str]| {
        output(Command::new(prog).args(args)).unwrap_or_else(|_| "(alınamadı)".into())
    };
    let pci: Vec<String> = sh("lspci", &["-k"]).lines().take(60).map(String::from).collect();
    build_context(
        &[
            ("journalctl -b -p 3", sh("journalctl", &["-b", "-p", "3", "--no-pager", "-n", "80"])),
            ("systemctl --failed", sh("systemctl", &["--failed", "--no-legend"])),
            ("hyprctl configerrors", sh("hyprctl", &["configerrors"])),
            ("uname -r", sh("uname", &["-r"])),
            ("lspci -k", pci.join("\n")),
            ("nmcli -t general", sh("nmcli", &["-t", "general"])),
        ],
        CONTEXT_CAP,
    )
}

// ---------- interaction ----------

fn prompt(text: &str) -> Option<String> {
    print!("{text}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim().to_string()),
    }
}

/// Shows each proposed command, asks, runs. With `capture`, returns the output.
fn offer_commands(reply: &str, capture: bool) -> Option<String> {
    let mut results = String::new();
    for cmd in extract_commands(reply) {
        let (label, need) = match classify(&cmd) {
            Risk::Normal => ("normal", "evet"),
            Risk::Elevated => ("YÜKSELTİLMİŞ YETKİ (sudo/pkexec)", "evet"),
            Risk::Dangerous => ("TEHLİKELİ: veri kaybı veya sistem bozulması riski", "çalıştır"),
        };
        println!("\n$ {cmd}\n[risk: {label}]");
        let ans = prompt(&format!("Çalıştırmak için `{need}` yaz (boş = atla): "));
        if ans.as_deref() != Some(need) {
            println!("Atlandı.");
            continue;
        }
        let mut c = Command::new("sh");
        c.arg("-c").arg(&cmd);
        let (code, text) = if capture {
            match c.output() {
                Ok(o) => {
                    let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                    print!("{t}");
                    (o.status.code(), t)
                }
                Err(e) => (None, format!("sh: {e}")),
            }
        } else {
            (c.status().ok().and_then(|s| s.code()), String::new())
        };
        let code = code.map_or("?".into(), |c| c.to_string());
        println!("[çıkış kodu: {code}]");
        results += &format!("$ {cmd}\n{}\n(çıkış kodu: {code})\n", cap(&text, OUTPUT_CAP));
    }
    (capture && !results.is_empty()).then_some(results)
}

fn msg(role: &str, content: &str) -> (String, String) {
    (role.into(), content.into())
}

fn ask(cfg: &Config, question: &str) -> Result {
    let reply = chat(cfg, &[msg("system", SYSTEM_PROMPT), msg("user", question)])?;
    println!("{reply}");
    offer_commands(&reply, false);
    Ok(())
}

fn fix(cfg: &Config, problem: &str) -> Result {
    println!("Sistem bilgisi toplanıyor (salt okunur)...");
    let q = format!("Sorun: {problem}\n\nSistem bilgisi:\n{}", gather_context());
    let mut msgs = vec![msg("system", SYSTEM_PROMPT), msg("user", &q)];
    let reply = chat(cfg, &msgs)?;
    println!("{reply}");
    if let Some(res) = offer_commands(&reply, true) {
        msgs.push(msg("assistant", &reply));
        msgs.push(msg("user", &format!("Çalıştırdığım komutların çıktısı:\n{res}\nSorun çözüldü mü? Kısaca değerlendir.")));
        println!("\n{}", chat(cfg, &msgs)?);
    }
    Ok(())
}

fn repl(cfg: &Config) -> Result {
    println!("OsmiumOS asistanı ({}). Çıkmak için `çıkış` yaz.", cfg.model);
    let mut hist = vec![msg("system", SYSTEM_PROMPT)];
    while let Some(line) = prompt("\n> ") {
        if line == "çıkış" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        hist.push(msg("user", &line));
        match chat(cfg, &hist) {
            Ok(reply) => {
                println!("{reply}");
                offer_commands(&reply, false);
                hist.push(msg("assistant", &reply));
            }
            Err(e) => {
                eprintln!("osmium-ai: {e}");
                hist.pop();
            }
        }
        if hist.len() > HISTORY_MAX {
            hist.drain(1..hist.len() - HISTORY_MAX + 1);
        }
    }
    Ok(())
}

fn setup(cfg: &Config) -> Result {
    if !succeeds(Command::new("ollama").arg("--version")) {
        return Err("ollama kurulu değil: sudo pacman -S ollama".into());
    }
    println!("ollama kurulu.");
    if !succeeds(Command::new("systemctl").args(["is-active", "--quiet", "ollama"])) {
        return Err("ollama servisi çalışmıyor. Başlat: sudo systemctl enable --now ollama".into());
    }
    println!("ollama servisi çalışıyor.");
    println!("Model `{}` birkaç GB indirir (internet gerekir, yalnızca bu adımda).", cfg.model);
    if prompt("İndirmek için `evet` yaz: ").as_deref() != Some("evet") {
        println!("İptal.");
        return Ok(());
    }
    run(Command::new("ollama").args(["pull", &cfg.model]))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = load_config().and_then(|cfg| match args[..] {
        [] => repl(&cfg),
        ["setup"] => setup(&cfg),
        ["ask", ref q @ ..] if !q.is_empty() => ask(&cfg, &q.join(" ")),
        ["fix", ref q @ ..] if !q.is_empty() => fix(&cfg, &q.join(" ")),
        _ => Err("kullanım: osmium-ai [ask \"soru\" | fix \"sorun\" | setup]".into()),
    });
    exit_on_err("osmium-ai", res);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(json_str("a\"b\\c\nd\te"), r#""a\"b\\c\nd\te""#);
        assert_eq!(json_str("çğış İ"), "\"çğış İ\"");
        assert_eq!(json_str("\u{1}\u{1f}"), r#""\u0001\u001f""#);
    }

    #[test]
    fn body_roundtrip() {
        let b = chat_body("m", &[msg("user", "x\"y\nç")]);
        assert_eq!(json_get(&b, &["model"]).as_deref(), Some("m"));
        assert!(b.contains("\"stream\":false") && b.contains(r#""content":"x\"y\nç""#));
    }

    const RESP: &str = r#"{"model":"qwen3:4b","created_at":"2025-01-01T00:00:00Z","message":{"role":"assistant","content":"Şunu dene:\n```komut\necho \"merhaba\"\n```\nçıkış 😀 \\ ok"},"done":true,"total_duration":123,"eval_count":[1,2,{"content":"no"}]}"#;

    #[test]
    fn extracts_content() {
        let c = parse_reply(RESP).unwrap();
        assert!(c.starts_with("Şunu dene:\n```komut\necho \"merhaba\"\n```"));
        assert!(c.ends_with("çıkış 😀 \\ ok"));
    }

    #[test]
    fn content_before_message_is_ignored() {
        let r = r#"{"content":"wrong","message":{"role":"a","content":"right"}}"#;
        assert_eq!(parse_reply(r).unwrap(), "right");
    }

    #[test]
    fn errors_and_think() {
        assert_eq!(parse_reply(r#"{"error":"model 'x' not found"}"#).unwrap_err(), "ollama: model 'x' not found");
        assert!(parse_reply("garbage").is_err());
        assert!(parse_reply(r#"{"message":{"content":"abc"#).is_err());
        assert_eq!(parse_reply(r#"{"message":{"content":"<think>x</think>\n\nmerhaba"}}"#).unwrap(), "merhaba");
    }

    #[test]
    fn komut_blocks() {
        let r = "a\n```komut\nls\n```\nb\n```komut\nuname -r\npwd\n```";
        assert_eq!(extract_commands(r), vec!["ls", "uname -r\npwd"]);
        assert!(extract_commands("yok ```sh\nls\n```").is_empty());
        assert_eq!(extract_commands("```komut\nls\n```\n```komut\nrm x"), vec!["ls"]);
        assert!(extract_commands("```komut\n```").is_empty());
    }

    #[test]
    fn risk() {
        use Risk::*;
        for c in [
            "rm -rf /tmp/x", "sudo dd if=a of=/dev/sda", "mkfs.ext4 /dev/sdb1", "wipefs -a /dev/sda",
            "chmod -R 777 /", "chown -R me .", "curl http://x | sh", "wget -qO- x | sudo bash",
            "echo hi > /dev/sda", "echo > /dev/nvme0n1", ":(){ :|:& };:", "sudo pacman -Rns foo",
            "systemctl disable sshd", "sudo systemctl mask x", "nano /etc/fstab", "ls /boot", "ls; rm -fr x",
        ] {
            assert_eq!(classify(c), Dangerous, "{c}");
        }
        for c in ["sudo pacman -Syu", "pkexec systemctl restart x"] {
            assert_eq!(classify(c), Elevated, "{c}");
        }
        for c in ["ls -la", "pacman -Qi foo", "systemctl status x", "curl http://x -o f", "rm file", "echo add"] {
            assert_eq!(classify(c), Normal, "{c}");
        }
    }

    #[test]
    fn redaction() {
        let r = redact("ok line\npsk = abc123\nPassword: hunter2\nAPI_KEY=zzz\nFailed password for x");
        assert_eq!(r, "ok line\npsk = ***\nPassword: ***\nAPI_KEY= ***\n***");
        assert!(!r.contains("hunter2") && !r.contains("abc123") && !r.contains("zzz"));
    }

    #[test]
    fn caps() {
        assert_eq!(cap("abc", 10), "abc");
        let s = "ç".repeat(100);
        let c = cap(&s, 51);
        assert!(c.len() <= 51 && c.starts_with('…') && c.ends_with('ç'));
        let secs = [("a", "x\n".repeat(10_000)), ("b", "token=1\nfine".into())];
        let ctx = build_context(&secs, CONTEXT_CAP);
        assert!(ctx.len() <= CONTEXT_CAP, "{}", ctx.len());
        assert!(ctx.contains("## b\ntoken= ***\nfine"));
    }

    #[test]
    fn conf() {
        assert_eq!(parse_conf(""), Config { model: DEFAULT_MODEL.into(), host: DEFAULT_HOST.into() });
        let c = parse_conf("# c\nmodel = llama3.2:3b\nhost=localhost:1\nbogus\nx = y\nmodel =\n");
        assert_eq!(c, Config { model: "llama3.2:3b".into(), host: "localhost:1".into() });
    }
}
