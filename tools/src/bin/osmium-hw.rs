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
    /// Turing or newer: supported by the open kernel modules. Carries the architecture.
    NvidiaOpen(&'static str),
    /// Older cards: the open modules do not support them; nouveau (already in mesa) stays.
    NvidiaLegacy(&'static str),
    Amd,
    Intel,
}

/// NVIDIA PCI device-id ranges by chip family: (first id, last id, architecture, open modules).
/// NVIDIA numbers ids per chip in blocks, so ranges are exact enough; ids are not monotonic
/// in age (GA100 sits between two Turing chips), which a single threshold got wrong.
const NVIDIA_ARCHES: &[(u32, u32, &str, bool)] = &[
    (0x0000, 0x0fbf, "Tesla/Fermi", false),
    (0x0fc0, 0x12ff, "Kepler", false),
    (0x1300, 0x17ff, "Maxwell", false),
    (0x1800, 0x1d7f, "Pascal", false),
    (0x1d80, 0x1dff, "Volta", false),
    (0x1e00, 0x1fff, "Turing", true),
    (0x2000, 0x20ff, "Ampere", true),
    (0x2100, 0x21ff, "Turing", true),
    (0x2200, 0x22ff, "Ampere", true),
    (0x2300, 0x23ff, "Hopper", true),
    (0x2400, 0x25ff, "Ampere", true),
    (0x2600, 0x28ff, "Ada Lovelace", true),
    (0x2900, 0x2fff, "Blackwell", true),
];

fn nvidia(device: u32) -> Gpu {
    match NVIDIA_ARCHES.iter().find(|(lo, hi, ..)| (*lo..=*hi).contains(&device)) {
        Some((.., arch, true)) => Gpu::NvidiaOpen(arch),
        Some((.., arch, false)) => Gpu::NvidiaLegacy(arch),
        // Ids past the table belong to chips newer than it, which the open modules support.
        None => Gpu::NvidiaOpen("yeni nesil"),
    }
}

/// A PCI device is a GPU when its class is 0x03xxxx (display controller).
fn classify(vendor: &str, device: &str, class: &str) -> Option<Gpu> {
    let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    if hex(class)? >> 16 != 0x03 {
        return None;
    }
    match hex(vendor)? {
        0x10de => Some(nvidia(hex(device)?)),
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
            Gpu::NvidiaOpen(_) => {
                // dkms: one driver package for whichever kernels are installed.
                w.extend(["nvidia-open-dkms", "nvidia-utils", "libva-nvidia-driver"].map(String::from));
                w.extend(kernels.iter().map(|k| format!("{k}-headers")));
            }
            Gpu::NvidiaLegacy(_) => {}
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

fn describe(g: Gpu) -> String {
    match g {
        Gpu::NvidiaOpen(arch) => format!("NVIDIA {arch} (açık çekirdek modülleriyle desteklenir)"),
        Gpu::NvidiaLegacy(arch) => {
            format!("NVIDIA {arch} (eski kart: nouveau sürücüsüyle çalışır, özel sürücü AUR'da)")
        }
        Gpu::Amd => "AMD".into(),
        Gpu::Intel => "Intel".into(),
    }
}

fn detect_gpus() -> Vec<Gpu> {
    let read = |p: std::path::PathBuf| fs::read_to_string(p).unwrap_or_default();
    let mut v: Vec<Gpu> = Vec::new();
    for e in fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
        let d = e.path();
        // Each kind once: two cards of one kind want the same packages.
        if let Some(g) = classify(&read(d.join("vendor")), &read(d.join("device")), &read(d.join("class")))
            && !v.contains(&g)
        {
            v.push(g);
        }
    }
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
        assert_eq!(classify("0x10de\n", "0x2684\n", "0x030000\n"), Some(Gpu::NvidiaOpen("Ada Lovelace")));
        assert_eq!(classify("0x10de", "0x1f82", "0x030200"), Some(Gpu::NvidiaOpen("Turing")));
        assert_eq!(classify("0x10de", "0x1c03", "0x030000"), Some(Gpu::NvidiaLegacy("Pascal")));
        assert_eq!(classify("0x1002", "0x73bf", "0x030000"), Some(Gpu::Amd));
        assert_eq!(classify("0x8086", "0x9a49", "0x030000"), Some(Gpu::Intel));
        // NVIDIA audio function of the same card, and a non-GPU vendor.
        assert_eq!(classify("0x10de", "0x22ba", "0x040300"), None);
        assert_eq!(classify("0x1234", "0x1111", "0x030000"), None);
        assert_eq!(classify("", "", ""), None);
    }

    #[test]
    fn nvidia_table() {
        // GTX 1660 (TU116) sits above GA100: both are open-module cards.
        assert_eq!(nvidia(0x2184), Gpu::NvidiaOpen("Turing"));
        assert_eq!(nvidia(0x20b0), Gpu::NvidiaOpen("Ampere"));
        assert_eq!(nvidia(0x2204), Gpu::NvidiaOpen("Ampere"));
        assert_eq!(nvidia(0x2b85), Gpu::NvidiaOpen("Blackwell"));
        assert_eq!(nvidia(0x3100), Gpu::NvidiaOpen("yeni nesil"));
        // Titan V (Volta) is below Turing but not supported by the open modules.
        assert_eq!(nvidia(0x1d81), Gpu::NvidiaLegacy("Volta"));
        assert_eq!(nvidia(0x13c2), Gpu::NvidiaLegacy("Maxwell"));
        assert_eq!(nvidia(0x1180), Gpu::NvidiaLegacy("Kepler"));
        // Contiguous and sorted, so every id below the end has exactly one family.
        for w in NVIDIA_ARCHES.windows(2) {
            assert_eq!(w[0].1 + 1, w[1].0, "{w:?}");
        }
    }

    #[test]
    fn packages() {
        let k = vec!["linux".to_string(), "linux-zen".to_string()];
        let w = wanted(&[Gpu::Intel, Gpu::NvidiaOpen("Ada Lovelace")], "none\n", &k);
        for p in ["nvidia-open-dkms", "nvidia-utils", "linux-headers", "linux-zen-headers", "vulkan-intel"] {
            assert!(w.iter().any(|x| x == p), "{p}");
        }
        assert!(wanted(&[Gpu::NvidiaLegacy("Pascal")], "none", &k).is_empty());
        assert_eq!(wanted(&[], "kvm\n", &k), ["qemu-guest-agent", "spice-vdagent"]);
        assert_eq!(wanted(&[Gpu::Amd], "oracle", &[]), ["virtualbox-guest-utils", "vulkan-radeon"]);
        assert_eq!(missing(&w, "nvidia-utils\nvulkan-intel\nlinux-headers\n").len(), w.len() - 3);
        // A longer name must not count as installed.
        assert_eq!(missing(&["mesa".to_string()], "mesa-utils\n"), ["mesa"]);
    }
}
