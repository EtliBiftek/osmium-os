//! Builds the OsmiumOS pacman repo and ISOs. Run as root on Arch (CI uses an archlinux container).
//!
//!   osmium-build packages        build packages/* and pinned AUR packages into ./repo
//!   osmium-build iso <edition>   build out/osmiumos-<edition>-*.iso
//!
//! Env: OSMIUM_REPO (published repo URL; enables package reuse and is used by the ISO),
//!      GITHUB_RUN_NUMBER (pkgrel for local packages).
//!
//! Package reuse: an AUR package is downloaded from the published repo when its file names
//! (`makepkg --packagelist`) are there. A local package is reused while its inputs (its
//! directory and what its PKGBUILD takes from the repo through $OSMIUM_SRC) hash the same:
//! the hash is published as `<name>.<hash>.built`, a list of the files built from it.
//! `work/fresh.txt` names the files of ./repo that did not come from the published repo.

use osmium_tools::{REPO_URL, Result, append, osmium_repo_section, output, remove_dir, remove_file, run, succeeds};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Appended to makepkg's config. No -debug split packages: nothing installs them, and with
/// `debug` on makepkg compiles Rust with full debug info. make uses every core.
const MAKEPKG_CONF: &str = "# Written by osmium-build.\nOPTIONS+=('!debug')\nMAKEFLAGS=\"-j$(nproc)\"\n";

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

/// Committed public key; when present the repo and ISO are signed and verified.
fn public_key(root: &Path) -> Option<PathBuf> {
    Some(root.join("keys/osmium.asc")).filter(|p| p.is_file())
}

/// Secret key id imported in CI (GitHub secret OSMIUM_GPG_KEY_ID).
fn signing_key() -> Option<String> {
    std::env::var("OSMIUM_GPG_KEY_ID").ok().filter(|k| !k.trim().is_empty())
}

fn fingerprint(key: &Path) -> Result<String> {
    let out = output(Command::new("gpg").args(["--with-colons", "--show-keys"]).arg(key))?;
    out.lines()
        .find_map(|l| l.strip_prefix("fpr:").and_then(|r| r.split(':').nth(8)).map(str::to_string))
        .ok_or_else(|| format!("{}: no fingerprint", key.display()))
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

/// OsmiumOS kernel, for the live ISO and therefore the installed system (the installer copies
/// the live root). Its headers come from osmium-hw, which reads the installed pkgbase.
const KERNEL: &str = "linux-zen";

/// releng's package list with `linux` replaced by KERNEL.
fn kernel_packages(list: &str) -> String {
    list.lines().map(|l| if l.trim() == "linux" { KERNEL } else { l }).map(|l| format!("{l}\n")).collect()
}

/// A releng boot menu (grub, systemd-boot, syslinux) pointed at KERNEL's image and initramfs.
fn kernel_boot_config(text: &str) -> String {
    if text.contains(&format!("vmlinuz-{KERNEL}")) {
        return text.to_string();
    }
    text.replace("vmlinuz-linux", &format!("vmlinuz-{KERNEL}"))
        .replace("initramfs-linux.img", &format!("initramfs-{KERNEL}.img"))
}

/// Swaps releng's `linux` for KERNEL in the package list and in every boot menu.
fn use_kernel(profile: &Path) -> Result {
    let list = profile.join("packages.x86_64");
    io(fs::write(&list, kernel_packages(&read(&list)?)), &list)?;
    let mut dirs: Vec<PathBuf> = ["grub", "efiboot", "syslinux"].iter().map(|d| profile.join(d)).collect();
    while let Some(dir) = dirs.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Ok(text) = fs::read_to_string(&path) {
                let new = kernel_boot_config(&text);
                if new != text {
                    io(fs::write(&path, new), &path)?;
                }
            }
        }
    }
    Ok(())
}

/// releng's live initramfs HOOKS line without `microcode`: mkarchiso then puts the ucode images
/// next to the kernel, where osmium-install finds them (see common/.../archiso.conf).
fn live_hooks(releng: &str) -> Option<String> {
    let line = releng.lines().map(str::trim).find(|l| l.starts_with("HOOKS=("))?;
    let list = line.strip_prefix("HOOKS=(")?.strip_suffix(')')?;
    let hooks: Vec<&str> = list.split_whitespace().filter(|h| *h != "microcode").collect();
    Some(format!("HOOKS=({})", hooks.join(" ")))
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
    let initcpio = profile.join("airootfs/etc/mkinitcpio.conf.d/archiso.conf");
    let hooks = live_hooks(&read(&initcpio)?).ok_or_else(|| format!("{}: no HOOKS line", initcpio.display()))?;

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
    // Our file replaces releng's; its HOOKS follow releng's, so archiso changes need no edit here.
    let ours = read(&initcpio)?;
    io(fs::write(&initcpio, format!("{hooks}\n{ours}")), &initcpio)?;
    use_kernel(&profile)?;
    let key = public_key(root);
    if let Some(key) = &key {
        // pacstrap uses the build host's keyring.
        // Idempotent; stock containers may lack the local master key --lsign-key needs.
        run(Command::new("pacman-key").arg("--init"))?;
        run(Command::new("pacman-key").arg("--add").arg(key))?;
        run(Command::new("pacman-key").args(["--lsign-key", &fingerprint(key)?]))?;
        append(profile.join("packages.x86_64"), "osmium-keyring
")?;
    }
    // What the ISO installs, for `osmium-apply export` to leave out.
    let base = profile.join("airootfs/usr/share/osmium/base-packages.txt");
    io(fs::create_dir_all(base.parent().unwrap()), &base)?;
    io(fs::copy(profile.join("packages.x86_64"), &base), &base)?;
    append(profile.join("pacman.conf"), &osmium_repo_section(&repo_url(), key.is_some()))?;
    let patch = read(&root.join("common/profiledef.patch.sh"))?;
    append(profile.join("profiledef.sh"), &format!("EDITION=\"{edition}\"\n{patch}"))?;
    let ed_patch = root.join("editions").join(edition).join("profiledef.patch.sh");
    if ed_patch.is_file() {
        append(profile.join("profiledef.sh"), &read(&ed_patch)?)?;
    }

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
    let conf = Path::new("/etc/makepkg.conf.d/osmium.conf");
    io(fs::create_dir_all(conf.parent().unwrap()), conf)?;
    io(fs::write(conf, MAKEPKG_CONF), conf)?;
    // Files of `out` that were downloaded from the published repo, not built here.
    let mut reused: BTreeSet<PathBuf> = BTreeSet::new();

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
        import_pgp_keys(&dir)?;
        build(root, &dir, &out, "-s", None, &mut reused)?;
    }

    // Our own packages. Built with --nodeps: CI preinstalls their build deps (rust).
    let rel = std::env::var("GITHUB_RUN_NUMBER").unwrap_or_else(|_| "1".into());
    let mut dirs: Vec<PathBuf> = io(fs::read_dir(root.join("packages")), "packages")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        // The keyring package needs the committed public key.
        .filter(|p| public_key(root).is_some() || !p.ends_with("osmium-keyring"))
        .collect();
    dirs.sort();
    for src in dirs {
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let dir = work.join(&name);
        run(Command::new("cp").arg("-a").arg(&src).arg(&dir))?;
        let pkgbuild = dir.join("PKGBUILD");
        let original = read(&pkgbuild)?;
        // A package rebuilt because its inputs changed gets this run's pkgrel, so pacman sees
        // the change as an upgrade; an unchanged one keeps the files it was published with.
        io(fs::write(&pkgbuild, set_pkgrel(&original, &rel)), &pkgbuild)?;
        let marker = match input_roots(&name, &original) {
            Some(roots) => Some(format!("{name}.{}.built", inputs_hash(root, &roots, &rebuild_salt(&original, week()))?)),
            None => None,
        };
        build(root, &dir, &out, "-d", marker.as_deref(), &mut reused)?;
    }

    // GitHub renames `:` in release asset names, so pacman would 404 on epoch packages.
    if let Some(p) = pkg_files(&out)?.iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(':'))) {
        return Err(format!("{}: epoch in file name, the GitHub release repo cannot serve it; repackage without an epoch", p.display()));
    }
    let key = signing_key();
    if public_key(root).is_some() && key.is_none() {
        return Err("keys/osmium.asc is committed but OSMIUM_GPG_KEY_ID is not set: refusing to publish an unsigned repo clients would reject".into());
    }
    let mut repo_add = Command::new("repo-add");
    if let Some(key) = &key {
        for pkg in pkg_files(&out)? {
            let sig = PathBuf::from(format!("{}.sig", pkg.display()));
            if !sig.is_file() {
                run(Command::new("gpg").args(["--batch", "--yes", "--detach-sign", "-u", key]).arg(&pkg))?;
            }
        }
        repo_add.args(["--sign", "--key", key]);
    }
    run(repo_add.arg(out.join("osmium.db.tar.gz")).args(pkg_files(&out)?))?;
    // GitHub release assets cannot be symlinks.
    let fresh_list = root.join("work/fresh.txt");
    for (link, target) in [
        ("db", "db.tar.gz"),
        ("files", "files.tar.gz"),
        ("db.sig", "db.tar.gz.sig"),
        ("files.sig", "files.tar.gz.sig"),
    ] {
        let (link, target) = (out.join(format!("osmium.{link}")), out.join(format!("osmium.{target}")));
        if target.is_file() {
            remove_file(&link)?;
            io(fs::copy(&target, &link), &link)?;
        }
    }
    // The publish step uploads only these: re-uploading an unchanged package would take it
    // offline for a moment for every installed system.
    let fresh: String = io(fs::read_dir(&out), &out)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && !reused.contains(p))
        .filter_map(|p| Some(format!("{}\n", p.file_name()?.to_str()?)))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    io(fs::write(&fresh_list, fresh), &fresh_list)
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

/// `validpgpkeys` fingerprints from an AUR checkout's .SRCINFO.
fn pgp_keys(srcinfo: &str) -> Vec<String> {
    srcinfo
        .lines()
        .filter_map(|l| l.trim().strip_prefix("validpgpkeys = "))
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_hexdigit()))
        .collect()
}

/// makepkg checks signed sources against the builder's keyring: fetch the keys the
/// PKGBUILD pins (the fingerprint is the trust anchor, the keyserver only delivers it).
fn import_pgp_keys(dir: &Path) -> Result {
    let keys = pgp_keys(&fs::read_to_string(dir.join(".SRCINFO")).unwrap_or_default());
    if keys.is_empty() {
        return Ok(());
    }
    for server in ["hkps://keyserver.ubuntu.com", "hkps://keys.openpgp.org"] {
        if succeeds(Command::new("sudo").args(["-u", "builder", "gpg", "--batch", "--keyserver", server, "--recv-keys"]).args(&keys)) {
            return Ok(());
        }
    }
    Err(format!("{}: could not fetch PGP keys {}", dir.display(), keys.join(" ")))
}

/// Repo entries a local package is built from: its own directory plus every top-level path
/// its PKGBUILD reaches through $OSMIUM_SRC. None when a use of $OSMIUM_SRC cannot be
/// resolved to a path: such a package is rebuilt every time.
fn input_roots(name: &str, pkgbuild: &str) -> Option<Vec<String>> {
    let mut roots = BTreeSet::from([format!("packages/{name}")]);
    for line in pkgbuild.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for (_, rest) in line.match_indices("OSMIUM_SRC").map(|(i, m)| line.split_at(i + m.len())) {
            // `$OSMIUM_SRC/x`, `${OSMIUM_SRC}/x`, `${OSMIUM_SRC:?message}/x`
            let rest = match rest.strip_prefix(':') {
                Some(r) => r.split_once('}')?.1,
                None => rest.strip_prefix('}').unwrap_or(rest),
            };
            let first = rest.strip_prefix('/')?.split(['/', '"', '\'', ' ', ')', ';', '$']).next()?;
            if first.is_empty() || first == ".." {
                return None;
            }
            roots.insert(first.to_string());
        }
    }
    Some(roots.into_iter().collect())
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

/// Every file and symlink under `path`, sorted; build output (`target`) and `.git` are skipped.
fn input_files(path: &Path, out: &mut Vec<PathBuf>) -> Result {
    let meta = io(fs::symlink_metadata(path), path)?;
    if !meta.is_dir() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = io(fs::read_dir(path), path)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for e in entries {
        if !e.file_name().is_some_and(|n| n == "target" || n == ".git") {
            input_files(&e, out)?;
        }
    }
    Ok(())
}

/// The bytes that identify a set of inputs: path, kind and content of every file.
fn inputs_manifest(root: &Path, roots: &[String]) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    for r in roots {
        input_files(&root.join(r), &mut files)?;
    }
    let mut m = Vec::new();
    for f in files {
        let meta = io(fs::symlink_metadata(&f), &f)?;
        let (kind, body) = if meta.is_symlink() {
            ('l', io(fs::read_link(&f), &f)?.to_string_lossy().into_owned().into_bytes())
        } else {
            (if executable(&meta) { 'x' } else { 'f' }, io(fs::read(&f), &f)?)
        };
        let rel = f.strip_prefix(root).unwrap_or(&f).to_string_lossy().replace('\\', "/");
        m.extend_from_slice(format!("{kind} {rel}\0{}\0", body.len()).as_bytes());
        m.extend_from_slice(&body);
    }
    Ok(m)
}

/// Weeks since 1970.
fn week() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / (7 * 86_400))
}

/// Compiled packages link against Arch's libraries, which move on without any input here
/// changing: they are rebuilt at least once a week. `any` packages have nothing to go stale.
fn rebuild_salt(pkgbuild: &str, week: u64) -> String {
    let any = pkgbuild.lines().any(|l| matches!(l.trim(), "arch=('any')" | "arch=(any)" | "arch=(\"any\")"));
    if any { String::new() } else { format!("week {week}\n") }
}

/// First 16 hex digits of the sha256 of `salt`, the makepkg settings and the manifest
/// (std has no hash that is stable across versions).
fn inputs_hash(root: &Path, roots: &[String], salt: &str) -> Result<String> {
    let mut manifest = format!("{salt}{MAKEPKG_CONF}").into_bytes();
    manifest.extend(inputs_manifest(root, roots)?);
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("sha256sum: {e}"))?;
    // sha256sum prints only after its input ends, so writing everything first cannot block.
    let written = child.stdin.take().ok_or("sha256sum: stdin")?.write_all(&manifest);
    let out = child.wait_with_output().map_err(|e| format!("sha256sum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match (written, text.get(..16)) {
        (Ok(()), Some(hex)) if out.status.success() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(hex.to_string()),
        _ => Err("sha256sum failed".into()),
    }
}

/// Package file names from a published `.built` marker; None unless every line is one.
fn parse_marker(text: &str) -> Option<Vec<String>> {
    let names: Vec<String> = text.lines().map(str::to_string).collect();
    let ok = |n: &String| n.ends_with(".pkg.tar.zst") && !n.starts_with('.') && !n.contains(['/', '\\', ':']);
    (!names.is_empty() && names.iter().all(ok)).then_some(names)
}

/// Downloads the named packages (and their signatures, where published) into `out`.
/// False, with nothing left behind, unless all of them are there.
fn download(out: &Path, names: &[String], reused: &mut BTreeSet<PathBuf>) -> Result<bool> {
    let mut got = Vec::new();
    let mut all = true;
    for name in names {
        let dest = out.join(name);
        let url = format!("{}/{name}", repo_url());
        got.push(dest.clone());
        if !succeeds(Command::new("curl").args(["-fsL", "--retry", "2", "-o"]).arg(&dest).arg(&url)) {
            all = false;
            break;
        }
        // Reuse its signature if one was published; otherwise it gets signed in packages().
        let sig = sig_path(&dest);
        got.push(sig.clone());
        if !succeeds(Command::new("curl").arg("-fsL").arg("-o").arg(&sig).arg(format!("{url}.sig"))) {
            remove_file(&sig)?;
        }
    }
    if !all {
        // Partial hit: drop every downloaded file and signature so a rebuilt package
        // never ships next to a signature for different bytes.
        for f in &got {
            remove_file(f)?;
        }
        return Ok(false);
    }
    reused.extend(got.into_iter().filter(|f| f.is_file()));
    Ok(true)
}

/// Builds one PKGBUILD dir into `out`, unless its packages are already published.
/// `marker` is the `.built` file name of a local package (see the module docs).
fn build(root: &Path, dir: &Path, out: &Path, flags: &str, marker: Option<&str>, reused: &mut BTreeSet<PathBuf>) -> Result {
    run(Command::new("chown").args(["-R", "builder"]).arg(dir))?;
    let published = if std::env::var("OSMIUM_REPO").is_err() {
        None
    } else if let Some(marker) = marker {
        output(Command::new("curl").args(["-fsL", "--retry", "2"]).arg(format!("{}/{marker}", repo_url())))
            .ok()
            .and_then(|text| parse_marker(&text))
    } else {
        let list = output(
            Command::new("sudo")
                .args(["-u", "builder", "makepkg", "--packagelist"])
                .current_dir(dir),
        )?;
        Some(list.lines().filter_map(|l| Some(Path::new(l).file_name()?.to_str()?.to_string())).collect())
    };
    if let Some(names) = published.filter(|n: &Vec<String>| !n.is_empty())
        && download(out, &names, reused)?
    {
        for name in &names {
            println!("cached: {name}");
        }
        write_marker(out, marker, &names)?;
        // Already published with exactly this content.
        reused.extend(marker.map(|m| out.join(m)));
        return Ok(());
    }
    run(Command::new("sudo")
        .args(["-u", "builder", "env"])
        .arg(format!("OSMIUM_SRC={}", root.display()))
        .args(["makepkg", flags, "-f", "--noconfirm"])
        .current_dir(dir))?;
    let mut built = Vec::new();
    for p in pkg_files(dir)? {
        let dest = out.join(p.file_name().unwrap());
        io(fs::rename(&p, &dest), &p)?;
        // New bytes: force a fresh signature in packages().
        remove_file(sig_path(&dest))?;
        built.push(p.file_name().unwrap().to_string_lossy().into_owned());
    }
    write_marker(out, marker, &built)
}

/// Publishes which files a local package's inputs produced (kept while they are reused).
fn write_marker(out: &Path, marker: Option<&str>, names: &[String]) -> Result {
    let Some(marker) = marker else { return Ok(()) };
    let path = out.join(marker);
    io(fs::write(&path, names.iter().map(|n| format!("{n}\n")).collect::<String>()), &path)
}

fn sig_path(pkg: &Path) -> PathBuf {
    let mut s = pkg.as_os_str().to_owned();
    s.push(".sig");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_is_swapped() {
        assert_eq!(kernel_packages("base\nlinux\nlinux-firmware\n"), "base\nlinux-zen\nlinux-firmware\n");
        let entry = "linux    /%INSTALL_DIR%/boot/%ARCH%/vmlinuz-linux\ninitrd   /%INSTALL_DIR%/boot/%ARCH%/initramfs-linux.img\n";
        let zen = kernel_boot_config(entry);
        assert_eq!(
            zen,
            "linux    /%INSTALL_DIR%/boot/%ARCH%/vmlinuz-linux-zen\ninitrd   /%INSTALL_DIR%/boot/%ARCH%/initramfs-linux-zen.img\n"
        );
        assert_eq!(kernel_boot_config(&zen), zen, "idempotent");
        let memtest = "linux /boot/memtest86+/memtest.efi\n";
        assert_eq!(kernel_boot_config(memtest), memtest);
    }

    #[test]
    fn live_hooks_follow_releng() {
        let releng = "# comment\nHOOKS=(base udev microcode modconf kms memdisk archiso block filesystems keyboard)\nCOMPRESSION=\"xz\"\n";
        assert_eq!(
            live_hooks(releng).as_deref(),
            Some("HOOKS=(base udev modconf kms memdisk archiso block filesystems keyboard)")
        );
        assert_eq!(live_hooks("COMPRESSION=\"xz\"\n"), None);
    }

    #[test]
    fn srcinfo_pgp_keys() {
        let s = "pkgbase = wlogout
	validpgpkeys = F4FDB18A9937358364B276E9E25D679AF73C6D2F
	validpgpkeys = nothex!
pkgname = wlogout
";
        assert_eq!(pgp_keys(s), ["F4FDB18A9937358364B276E9E25D679AF73C6D2F"]);
        assert!(pgp_keys("pkgbase = x
").is_empty());
    }

    #[test]
    fn pkgrel_replaced_only_on_its_line() {
        let s = set_pkgrel("pkgname=x\npkgrel=1\n# pkgrel=9 in comment\n", "42");
        assert_eq!(s, "pkgname=x\npkgrel=42\n# pkgrel=9 in comment\n");
    }

    #[test]
    fn inputs_named_by_the_pkgbuild() {
        let roots = |name: &str, text: &str| input_roots(name, text);
        assert_eq!(roots("ttf-lexend", "pkgname=ttf-lexend\n").unwrap(), ["packages/ttf-lexend"]);
        let tools = "# osmium-build sets OSMIUM_SRC to the repo root.\n\
                     build() { cargo build --manifest-path \"${OSMIUM_SRC:?build with osmium-build}/tools/Cargo.toml\"; }\n\
                     package() {\n  install -Dm644 \"$OSMIUM_SRC/LICENSE\" -t x\n  local data=\"$OSMIUM_SRC/tools/data\"\n  cp ${OSMIUM_SRC}/keys/osmium.asc x\n}\n";
        assert_eq!(roots("osmium-tools", tools).unwrap(), ["LICENSE", "keys", "packages/osmium-tools", "tools"]);
        // Uses that name no path cannot be tracked: never reuse such a package.
        assert_eq!(roots("x", "package() { cd \"$OSMIUM_SRC\"; }"), None);
        assert_eq!(roots("x", "package() { cp -r \"$OSMIUM_SRC/\" x; }"), None);
        assert_eq!(roots("x", "package() { cat $OSMIUM_SRC/../secret; }"), None);
    }

    #[test]
    fn every_shipped_pkgbuild_has_trackable_inputs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut seen = 0;
        for e in fs::read_dir(root.join("packages")).unwrap().flatten().filter(|e| e.path().is_dir()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let roots = input_roots(&name, &fs::read_to_string(e.path().join("PKGBUILD")).unwrap());
            let roots = roots.unwrap_or_else(|| panic!("{name}: $OSMIUM_SRC use without a path"));
            for r in &roots {
                // keys/osmium.asc is optional; everything else a PKGBUILD names must exist.
                assert!(root.join(r).exists() || r == "keys", "{name}: {r} is not in the repo");
            }
            seen += 1;
        }
        assert!(seen >= 8);
    }

    #[test]
    fn manifest_changes_with_content_name_and_mode() {
        let d = std::env::temp_dir().join(format!("osmium-build-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("packages/a/overlay")).unwrap();
        fs::create_dir_all(d.join("tools/target/debug")).unwrap();
        fs::write(d.join("packages/a/PKGBUILD"), "pkgname=a\n").unwrap();
        fs::write(d.join("packages/a/overlay/x"), "1").unwrap();
        fs::write(d.join("tools/main.rs"), "fn main() {}").unwrap();
        fs::write(d.join("tools/target/debug/junk"), "build output").unwrap();
        let roots = ["packages/a".to_string(), "tools".to_string()];
        let base = inputs_manifest(&d, &roots).unwrap();
        assert_eq!(inputs_manifest(&d, &roots).unwrap(), base, "stable");
        let text = String::from_utf8_lossy(&base).into_owned();
        assert!(text.contains("f packages/a/overlay/x\u{0}1\u{0}1") && text.contains("tools/main.rs"), "{text:?}");
        assert!(!text.contains("junk"), "build output is not an input");

        fs::write(d.join("tools/target/debug/junk"), "other").unwrap();
        assert_eq!(inputs_manifest(&d, &roots).unwrap(), base);
        fs::write(d.join("packages/a/overlay/x"), "2").unwrap();
        let edited = inputs_manifest(&d, &roots).unwrap();
        assert_ne!(edited, base);
        fs::rename(d.join("packages/a/overlay/x"), d.join("packages/a/overlay/y")).unwrap();
        assert_ne!(inputs_manifest(&d, &roots).unwrap(), edited);
        assert!(inputs_manifest(&d, &["missing".to_string()]).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let before = inputs_manifest(&d, &roots).unwrap();
            fs::set_permissions(d.join("packages/a/overlay/y"), fs::Permissions::from_mode(0o755)).unwrap();
            assert_ne!(inputs_manifest(&d, &roots).unwrap(), before);
            let h = inputs_hash(&d, &roots, "").unwrap();
            assert!(h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()), "{h}");
            assert_eq!(inputs_hash(&d, &roots, "").unwrap(), h);
            assert_ne!(inputs_hash(&d, &roots, "week 1\n").unwrap(), h);
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn compiled_packages_are_rebuilt_weekly() {
        assert_eq!(rebuild_salt("pkgname=a\narch=('any')\n", 7), "");
        assert_eq!(rebuild_salt("pkgname=a\narch=('x86_64')\n", 7), "week 7\n");
        assert_ne!(rebuild_salt("arch=('x86_64')", 7), rebuild_salt("arch=('x86_64')", 8));
        assert!(week() > 2900);
    }

    #[test]
    fn marker_lists_package_files_only() {
        assert_eq!(
            parse_marker("a-1-2-any.pkg.tar.zst\nb-debug-1-2-x86_64.pkg.tar.zst\n").unwrap(),
            ["a-1-2-any.pkg.tar.zst", "b-debug-1-2-x86_64.pkg.tar.zst"]
        );
        for bad in ["", "\n", "<html>Not Found</html>", "../a.pkg.tar.zst\n", "a.pkg.tar.zst\nosmium.db\n", "a-1:2-1-any.pkg.tar.zst"] {
            assert_eq!(parse_marker(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn makepkg_conf_is_valid_shell_text() {
        assert!(MAKEPKG_CONF.contains("OPTIONS+=('!debug')\n") && MAKEPKG_CONF.contains("MAKEFLAGS=\"-j$(nproc)\"\n"));
    }

    #[test]
    fn sig_path_appends_suffix() {
        assert_eq!(sig_path(Path::new("repo/a-1-1-x86_64.pkg.tar.zst")), Path::new("repo/a-1-1-x86_64.pkg.tar.zst.sig"));
    }
}
