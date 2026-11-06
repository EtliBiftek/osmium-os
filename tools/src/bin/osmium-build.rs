//! Builds the OsmiumOS pacman repo and ISOs. Run as root on Arch (CI uses an archlinux container).
//!
//!   osmium-build packages        build packages/* and pinned AUR packages into ./repo
//!   osmium-build iso <edition>   build out/osmiumos-<edition>-*.iso
//!
//! Env: OSMIUM_REPO (published repo URL; enables package reuse and is used by the ISO),
//!      GITHUB_RUN_NUMBER (pkgrel for local packages).

use osmium_tools::{
    REPO_URL, Result, append, osmium_repo_section, output, remove_dir, remove_file, run, succeeds,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["packages"] => packages(&root),
        ["iso", edition] => iso(&root, edition),
        _ => Err("usage: osmium-build packages | osmium-build iso <edition>".into()),
    };
    if let Err(e) = res {
        eprintln!("osmium-build: {e}");
        std::process::exit(1);
    }
}

fn repo_url() -> String {
    std::env::var("OSMIUM_REPO").unwrap_or_else(|_| REPO_URL.into())
}

fn io<T>(res: std::io::Result<T>, what: impl AsRef<Path>) -> Result<T> {
    res.map_err(|e| format!("{}: {e}", what.as_ref().display()))
}

fn read(path: &Path) -> Result<String> {
    io(fs::read_to_string(path), path)
}

fn iso(root: &Path, edition: &str) -> Result {
    let ed = root.join("editions").join(edition);
    if !ed.is_dir() {
        return Err(format!("unknown edition: {edition}"));
    }
    let work = root.join("work");
    let profile = work.join(format!("profile-{edition}"));
    remove_dir(&profile)?;
    io(fs::create_dir_all(&work), &work)?;
    run(Command::new("cp").args(["-a", "/usr/share/archiso/configs/releng"]).arg(&profile))?;

    for layer in [root.join("common"), ed] {
        let overlay = layer.join("airootfs");
        if overlay.is_dir() {
            run(Command::new("cp").arg("-a").arg(overlay.join(".")).arg(profile.join("airootfs")))?;
        }
        let pkgs = layer.join("packages.x86_64");
        if pkgs.is_file() {
            append(profile.join("packages.x86_64"), &read(&pkgs)?)?;
        }
    }
    append(profile.join("pacman.conf"), &osmium_repo_section(&repo_url()))?;
    let patch = read(&root.join("common/profiledef.patch.sh"))?;
    append(profile.join("profiledef.sh"), &format!("EDITION=\"{edition}\"\n{patch}"))?;

    run(Command::new("mkarchiso")
        .arg("-v")
        .arg("-w")
        .arg(work.join(format!("build-{edition}")))
        .arg("-o")
        .arg(root.join("out"))
        .arg(&profile))
}

fn packages(root: &Path) -> Result {
    let out = root.join("repo");
    let work = root.join("work/pkg");
    for d in [&out, &work] {
        remove_dir(d)?;
        io(fs::create_dir_all(d), d)?;
    }
    if !succeeds(Command::new("id").arg("builder")) {
        run(Command::new("useradd").args(["-m", "builder"]))?;
    }
    let sudoers = Path::new("/etc/sudoers.d/builder");
    io(fs::write(sudoers, "builder ALL=(ALL) NOPASSWD: /usr/bin/pacman\n"), sudoers)?;

    // AUR packages, pinned to reviewed commits.
    for line in read(&root.join("packages/aur.txt"))?.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(commit)) = (parts.next(), parts.next()) else { continue };
        if name.starts_with('#') {
            continue;
        }
        let dir = work.join(name);
        run(Command::new("git")
            .args(["clone", "-q", &format!("https://aur.archlinux.org/{name}.git")])
            .arg(&dir))?;
        run(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "advice.detachedHead=false", "checkout", "-q", commit]))?;
        build(root, &dir, &out, "-s")?;
    }

    // Our own packages. Built with --nodeps: CI preinstalls their build deps (rust).
    let rel = std::env::var("GITHUB_RUN_NUMBER").unwrap_or_else(|_| "1".into());
    let mut dirs: Vec<PathBuf> = io(fs::read_dir(root.join("packages")), "packages")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for src in dirs {
        let dir = work.join(src.file_name().unwrap());
        run(Command::new("cp").arg("-a").arg(&src).arg(&dir))?;
        let pkgbuild = dir.join("PKGBUILD");
        // Every CI run gets a new pkgrel so pacman sees config changes as upgrades.
        let text = set_pkgrel(&read(&pkgbuild)?, &rel);
        io(fs::write(&pkgbuild, text), &pkgbuild)?;
        build(root, &dir, &out, "-d")?;
    }

    // Calamares silently drops its Python modules when Python is missing at build time.
    let calamares = pkg_files(&out)?
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("calamares-"))
        .ok_or("calamares package missing")?;
    if !output(Command::new("bsdtar").arg("-tf").arg(&calamares))?.contains("modules/bootloader/main.py") {
        return Err("calamares package has no Python modules".into());
    }

    run(Command::new("repo-add").arg(out.join("osmium.db.tar.gz")).args(pkg_files(&out)?))?;
    // GitHub release assets cannot be symlinks.
    for f in ["db", "files"] {
        let link = out.join(format!("osmium.{f}"));
        remove_file(&link)?;
        io(fs::copy(out.join(format!("osmium.{f}.tar.gz")), &link), &link)?;
    }
    Ok(())
}

fn set_pkgrel(pkgbuild: &str, rel: &str) -> String {
    pkgbuild
        .lines()
        .map(|l| if l.starts_with("pkgrel=") { format!("pkgrel={rel}\n") } else { format!("{l}\n") })
        .collect()
}

fn pkg_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = io(fs::read_dir(dir), dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".pkg.tar.zst"))
        .collect();
    v.sort();
    Ok(v)
}

/// Builds one PKGBUILD dir into `out`, unless every output file is already published.
fn build(root: &Path, dir: &Path, out: &Path, flags: &str) -> Result {
    run(Command::new("chown").args(["-R", "builder"]).arg(dir))?;
    let list = output(
        Command::new("sudo")
            .args(["-u", "builder", "makepkg", "--packagelist"])
            .current_dir(dir),
    )?;
    let use_cache = std::env::var("OSMIUM_REPO").is_ok();
    let mut cached = true;
    for name in list.lines().filter_map(|l| Path::new(l).file_name()) {
        let dest = out.join(name);
        let url = format!("{}/{}", repo_url(), name.to_string_lossy());
        if use_cache && succeeds(Command::new("curl").args(["-fsL", "-o"]).arg(&dest).arg(&url)) {
            println!("cached: {}", name.to_string_lossy());
        } else {
            remove_file(&dest)?;
            cached = false;
        }
    }
    if cached {
        return Ok(());
    }
    run(Command::new("sudo")
        .args(["-u", "builder", "env"])
        .arg(format!("OSMIUM_SRC={}", root.display()))
        .args(["makepkg", flags, "-f", "--noconfirm"])
        .current_dir(dir))?;
    for p in pkg_files(dir)? {
        let dest = out.join(p.file_name().unwrap());
        io(fs::rename(&p, &dest), &p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkgrel_replaced_only_on_its_line() {
        let s = set_pkgrel("pkgname=x\npkgrel=1\n# pkgrel=9 in comment\n", "42");
        assert_eq!(s, "pkgname=x\npkgrel=42\n# pkgrel=9 in comment\n");
    }
}
