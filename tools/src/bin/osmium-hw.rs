//! Hardware detection: finds the drivers this machine needs and installs them.
//!
//!   osmium-hw        show what was detected and which packages are missing
//!   osmium-hw kur    install the missing packages (asks first)

use osmium_tools::{Result, exit_on_err, output, run, succeeds};
use std::fs;
use std::io::{BufRead, Write};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Gpu {
    /// Turing or newer: supported by the open kernel modules.
    NvidiaOpen,
    /// Older cards: the open modules do not support them; nouveau (already in mesa) stays.
    NvidiaLegacy,
    Amd,
    Intel,
}

/// A PCI device is a GPU when its class is 0x03xxxx (display controller).
fn classify(vendor: &str, device: &str, class: &str) -> Option<Gpu> {
    let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    if hex(class)? >> 16 != 0x03 {
        return None;
    }
    match hex(vendor)? {
        // shortcut: device ids from 0x1e00 are Turing (GTX 16xx/RTX 20xx) and newer; a few
        // older ids above it would be misjudged. Switch to an id table if that ever bites.
        0x10de => Some(if hex(device)? >= 0x1e00 { Gpu::NvidiaOpen } else { Gpu::NvidiaLegacy }),
        0x1002 => Some(Gpu::Amd),
        0x8086 => Some(Gpu::Intel),
        _ => None,
    }
}

/// Packages this hardware wants. `kernels` are the installed kernel package names (pkgbase).
fn wanted(gpus: &[Gpu], virt: &str, kernels: &[String]) -> Vec<String> {
    let mut w: Vec<String> = Vec::new();
    for g in gpus {
        match g {
            Gpu::NvidiaOpen => {
                // dkms: one driver package for whichever kernels are installed.
                w.extend(["nvidia-open-dkms", "nvidia-utils", "libva-nvidia-driver"].map(String::from));
                w.extend(kernels.iter().map(|k| format!("{k}-headers")));
            }
            Gpu::NvidiaLegacy => {}
            // VA-API for AMD ships inside mesa.
            Gpu::Amd => w.push("vulkan-radeon".into()),
            Gpu::Intel => w.extend(["vulkan-intel", "intel-media-driver"].map(String::from)),
        }
    }
    match virt.trim() {
        "kvm" | "qemu" => w.extend(["qemu-guest-agent", "spice-vdagent"].map(String::from)),
        "oracle" => w.push("virtualbox-guest-utils".into()),
        "vmware" => w.push("open-vm-tools".into()),
        _ => {}
    }
    w.sort();
    w.dedup();
    w
}

fn missing(wanted: &[String], installed: &str) -> Vec<String> {
    wanted.iter().filter(|p| !installed.lines().any(|l| l == p.as_str())).cloned().collect()
}

fn describe(g: Gpu) -> &'static str {
    match g {
        Gpu::NvidiaOpen => "NVIDIA (açık çekirdek modülleriyle desteklenir)",
        Gpu::NvidiaLegacy => "NVIDIA (eski kart: nouveau sürücüsüyle çalışır, özel sürücü AUR'da)",
        Gpu::Amd => "AMD",
        Gpu::Intel => "Intel",
    }
}

fn detect_gpus() -> Vec<Gpu> {
    let read = |p: std::path::PathBuf| fs::read_to_string(p).unwrap_or_default();
    let mut v: Vec<Gpu> = fs::read_dir("/sys/bus/pci/devices")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            classify(&read(d.join("vendor")), &read(d.join("device")), &read(d.join("class")))
        })
        .collect();
    v.dedup();
    v
}

fn kernels() -> Vec<String> {
    fs::read_dir("/usr/lib/modules")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("pkgbase")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn main() {
    let install = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("kur") => true,
        Some(_) => {
            eprintln!("kullanım: osmium-hw [kur]");
            std::process::exit(2);
        }
    };
    exit_on_err("osmium-hw", hw(install));
}

fn hw(install: bool) -> Result {
    let gpus = detect_gpus();
    let virt = output(&mut Command::new("systemd-detect-virt")).unwrap_or_default();
    for g in &gpus {
        println!("Ekran kartı: {}", describe(*g));
    }
    if gpus.is_empty() {
        println!("Ekran kartı algılanamadı.");
    }
    if !matches!(virt.trim(), "" | "none") {
        println!("Sanal makine: {}", virt.trim());
    }
    let installed = output(Command::new("pacman").arg("-Qq"))?;
    let need = missing(&wanted(&gpus, &virt, &kernels()), &installed);
    if need.is_empty() {
        println!("Gerekli sürücüler kurulu.");
        return Ok(());
    }
    println!("Eksik paketler: {}", need.join(" "));
    if !install {
        println!("Kurmak için: osmium-hw kur");
        return Ok(());
    }
    print!("Kurmak için `evet` yaz: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if line.trim() != "evet" {
        println!("İptal.");
        return Ok(());
    }
    run(Command::new("sudo").args(["pacman", "-S", "--needed"]).args(&need))?;
    if need.iter().any(|p| p == "qemu-guest-agent") {
        succeeds(Command::new("sudo").args(["systemctl", "enable", "--now", "qemu-guest-agent.service"]));
    }
    if need.iter().any(|p| p.starts_with("nvidia")) {
        println!("NVIDIA sürücüsü yeniden başlatınca etkin olur.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_classes() {
        assert_eq!(classify("0x10de\n", "0x2684\n", "0x030000\n"), Some(Gpu::NvidiaOpen));
        assert_eq!(classify("0x10de", "0x1f82", "0x030200"), Some(Gpu::NvidiaOpen));
        assert_eq!(classify("0x10de", "0x1c03", "0x030000"), Some(Gpu::NvidiaLegacy));
        assert_eq!(classify("0x1002", "0x73bf", "0x030000"), Some(Gpu::Amd));
        assert_eq!(classify("0x8086", "0x9a49", "0x030000"), Some(Gpu::Intel));
        // NVIDIA audio function of the same card, and a non-GPU vendor.
        assert_eq!(classify("0x10de", "0x22ba", "0x040300"), None);
        assert_eq!(classify("0x1234", "0x1111", "0x030000"), None);
        assert_eq!(classify("", "", ""), None);
    }

    #[test]
    fn packages() {
        let k = vec!["linux".to_string(), "linux-zen".to_string()];
        let w = wanted(&[Gpu::Intel, Gpu::NvidiaOpen], "none\n", &k);
        for p in ["nvidia-open-dkms", "nvidia-utils", "linux-headers", "linux-zen-headers", "vulkan-intel"] {
            assert!(w.iter().any(|x| x == p), "{p}");
        }
        assert!(wanted(&[Gpu::NvidiaLegacy], "none", &k).is_empty());
        assert_eq!(wanted(&[], "kvm\n", &k), ["qemu-guest-agent", "spice-vdagent"]);
        assert_eq!(wanted(&[Gpu::Amd], "oracle", &[]), ["virtualbox-guest-utils", "vulkan-radeon"]);
        assert_eq!(missing(&w, "nvidia-utils\nvulkan-intel\nlinux-headers\n").len(), w.len() - 3);
        // A longer name must not count as installed.
        assert_eq!(missing(&["mesa".to_string()], "mesa-utils\n"), ["mesa"]);
    }
}
