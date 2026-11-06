//! Config time machine: keeps every version of ~/.config in a private git
//! history, so "it worked yesterday" is one command away.
//!
//!   osmium-history snapshot [message]   record changes (a user timer runs this)
//!   osmium-history log [path]           list snapshots, newest first
//!   osmium-history diff <id> [path]     what changed since snapshot <id>
//!   osmium-history restore <id> [path]  put ~/.config (or path) back to <id>
//!
//! History lives in ~/.local/share/osmium/config-history (mode 700). It never
//! leaves the machine, but it does keep old copies of whatever was in ~/.config.

use osmium_tools::{Result, config_home, data_home, exit_on_err, output, run, succeeds};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Big, volatile or secret paths under ~/.config that do not belong in history.
const EXCLUDES: &[&str] = &[
    "/chromium/",
    "/google-chrome/",
    "/BraveSoftware/",
    "/vivaldi/",
    "/microsoft-edge/",
    "/discord/",
    "/Code/",
    "/VSCodium/",
    "/Slack/",
    "/spotify/",
    "/obs-studio/logs/",
    "/pulse/",
    "/gh/hosts.yml",
    "/rclone/rclone.conf",
    "*.sock",
    "*.lock",
    "*.log",
    "**/Cache/",
    "**/cache/",
    "**/GPUCache/",
];

struct Repo {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl Repo {
    fn open() -> Result<Self> {
        let repo = Repo {
            git_dir: data_home()?.join("osmium/config-history"),
            work_tree: config_home()?,
        };
        if !repo.git_dir.join("HEAD").is_file() {
            repo.init()?;
        }
        Ok(repo)
    }

    fn git(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("--git-dir").arg(&self.git_dir).arg("--work-tree").arg(&self.work_tree);
        c.current_dir(&self.work_tree);
        c
    }

    fn init(&self) -> Result {
        fs::create_dir_all(&self.git_dir).map_err(|e| format!("{}: {e}", self.git_dir.display()))?;
        run(Command::new("chmod").arg("700").arg(&self.git_dir))?;
        run(self.git().args(["init", "-q", "-b", "main"]))?;
        for (k, v) in [
            ("user.name", "osmium-history"),
            ("user.email", "osmium-history@localhost"),
            ("commit.gpgsign", "false"),
            ("core.quotepath", "false"),
        ] {
            run(self.git().args(["config", k, v]))?;
        }
        let exclude = self.git_dir.join("info/exclude");
        fs::create_dir_all(exclude.parent().unwrap()).map_err(|e| format!("{}: {e}", exclude.display()))?;
        fs::write(&exclude, EXCLUDES.join("\n") + "\n").map_err(|e| format!("{}: {e}", exclude.display()))
    }

    /// Commits pending changes. Returns false when there was nothing to record.
    fn snapshot(&self, message: &str) -> Result<bool> {
        run(self.git().args(["add", "-A"]))?;
        let has_head = succeeds(self.git().args(["rev-parse", "-q", "--verify", "HEAD"]));
        if has_head && succeeds(self.git().args(["diff", "--cached", "--quiet"])) {
            return Ok(false);
        }
        run(self.git().args(["commit", "-q", "--allow-empty-message", "-m", message]))?;
        Ok(true)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exit_on_err("osmium-history", dispatch(&args));
}

fn dispatch(args: &[&str]) -> Result {
    let repo = Repo::open()?;
    match args {
        ["snapshot"] => snapshot(&repo, "auto"),
        ["snapshot", msg @ ..] => snapshot(&repo, &msg.join(" ")),
        ["log", path @ ..] if path.len() <= 1 => {
            let mut git = repo.git();
            git.args(["log", "-n", "50", "--date=format:%Y-%m-%d %H:%M", "--format=%h  %ad  %s", "--stat=80"]);
            if let [p] = path {
                git.args(["--", p]);
            }
            print!("{}", output(&mut git)?);
            Ok(())
        }
        ["diff", id, path @ ..] if path.len() <= 1 => {
            let mut git = repo.git();
            git.args(["diff", "--stat", "-p", id, "--"]).args(path);
            print!("{}", output(&mut git)?);
            Ok(())
        }
        ["restore", id, path @ ..] if path.len() <= 1 => restore(&repo, id, path.first().copied()),
        _ => Err("usage: osmium-history snapshot [message] | log [path] | diff <id> [path] | restore <id> [path]".into()),
    }
}

fn snapshot(repo: &Repo, message: &str) -> Result {
    if repo.snapshot(message)? {
        println!("snapshot saved");
    }
    Ok(())
}

fn restore(repo: &Repo, id: &str, path: Option<&str>) -> Result {
    if !succeeds(repo.git().args(["rev-parse", "-q", "--verify", &format!("{id}^{{commit}}")])) {
        return Err(format!("unknown snapshot `{id}` (see: osmium-history log)"));
    }
    // Restoring is itself undoable: record the current state first.
    repo.snapshot(&format!("before restore {id}"))?;
    let target = path.unwrap_or(".");
    run(repo.git().args(["restore", "--source", id, "--staged", "--worktree", "--", target]))?;
    repo.snapshot(&format!("restore {id} {target}"))?;
    println!("restored {target} to {id}");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> Repo {
        let base = std::env::temp_dir().join(format!("osmium-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let work_tree = base.join("config");
        fs::create_dir_all(&work_tree).unwrap();
        let repo = Repo { git_dir: base.join("history"), work_tree };
        repo.init().unwrap();
        repo
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let repo = temp_repo("roundtrip");
        let f = repo.work_tree.join("hypr/hyprland.lua");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, "good").unwrap();
        assert!(repo.snapshot("v1").unwrap());
        assert!(!repo.snapshot("nothing").unwrap(), "no change, no commit");
        let v1 = output(repo.git().args(["rev-parse", "--short", "HEAD"])).unwrap();

        fs::write(&f, "broken").unwrap();
        fs::write(repo.work_tree.join("new.conf"), "x").unwrap();
        repo.snapshot("v2").unwrap();

        restore(&repo, v1.trim(), None).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "good");
        assert!(!repo.work_tree.join("new.conf").exists(), "files added later are removed");
        // The broken state is still in history.
        let log = output(repo.git().args(["log", "--format=%s"])).unwrap();
        assert!(log.contains("v2") && log.contains("before restore"));
    }

    #[test]
    fn excluded_paths_not_recorded() {
        let repo = temp_repo("exclude");
        fs::create_dir_all(repo.work_tree.join("gh")).unwrap();
        fs::write(repo.work_tree.join("gh/hosts.yml"), "token").unwrap();
        fs::write(repo.work_tree.join("keep.conf"), "x").unwrap();
        repo.snapshot("s").unwrap();
        let files = output(repo.git().args(["ls-files"])).unwrap();
        assert!(files.contains("keep.conf") && !files.contains("hosts.yml"));
    }

    #[test]
    fn unknown_snapshot_rejected() {
        let repo = temp_repo("unknown");
        fs::write(repo.work_tree.join("a"), "x").unwrap();
        repo.snapshot("s").unwrap();
        assert!(restore(&repo, "deadbeef", None).unwrap_err().contains("unknown snapshot"));
    }
}
