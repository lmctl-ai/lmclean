//! Read-only Git working-tree inspection for `File` methods (`is_git_repo`,
//! `is_git_clean`, `is_git_pushed`, `git_status`).
//!
//! Conservative by design: a directory is only reported as `"clean-pushed"`
//! (`is_git_clean() && is_git_pushed()`) when it is the root of a non-bare Git
//! working tree with zero uncommitted/untracked changes, zero stashes, no
//! in-progress merge/rebase/cherry-pick/revert/bisect, at least one valid
//! remote URL, a named current branch whose upstream (or remote counterpart)
//! contains `HEAD` with 0 commits ahead, and zero commits on any local branch
//! that are not reachable from a valid remote's tracking refs.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitInfo {
    pub is_repo: bool,
    pub is_clean: bool,
    pub is_pushed: bool,
    pub status: &'static str,
}

impl GitInfo {
    pub const fn not_git() -> Self {
        Self {
            is_repo: false,
            is_clean: false,
            is_pushed: false,
            status: "not-git",
        }
    }

    const fn dirty() -> Self {
        Self {
            is_repo: true,
            is_clean: false,
            is_pushed: false,
            status: "dirty",
        }
    }

    const fn no_remote() -> Self {
        Self {
            is_repo: true,
            is_clean: true,
            is_pushed: false,
            status: "no-remote",
        }
    }

    const fn unpushed() -> Self {
        Self {
            is_repo: true,
            is_clean: true,
            is_pushed: false,
            status: "unpushed",
        }
    }

    const fn clean_pushed() -> Self {
        Self {
            is_repo: true,
            is_clean: true,
            is_pushed: true,
            status: "clean-pushed",
        }
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-c")
        .arg("core.trustctime=false")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Returns true when `url` is a real remote URL (`https://`, `http://`,
/// `ssh://`, `git://`, `file://`, or SCP-style `user@host:path` / `host:path`)
/// and not a bare local filesystem path (`/path`, `./path`, `../path`, `C:\...`)
/// or a disabled placeholder (`DISABLED*`).
pub fn is_valid_remote_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() || u.to_ascii_uppercase().starts_with("DISABLED") {
        return false;
    }
    for scheme in ["https://", "http://", "ssh://", "git://", "file://"] {
        if let Some(rest) = u.strip_prefix(scheme) {
            return !rest.is_empty();
        }
    }
    if u.starts_with('/') || u.starts_with("./") || u.starts_with("../") || u.contains('\\') {
        return false;
    }
    // SCP-style syntax: [user@]host:path where `host` has no `/` and is not a
    // single Windows drive letter (`C:`).
    if let Some((host_part, path_part)) = u.split_once(':') {
        if !host_part.is_empty()
            && !path_part.is_empty()
            && !host_part.contains('/')
            && host_part.len() > 1
        {
            return true;
        }
    }
    false
}

fn has_in_progress_op(git_dir: &Path) -> bool {
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
    ] {
        if fs::symlink_metadata(git_dir.join(marker)).is_ok() {
            return true;
        }
    }
    false
}

pub fn inspect(path: &Path) -> GitInfo {
    // Fast path: only inspect directories that actually have a `.git` entry.
    let Ok(meta) = fs::symlink_metadata(path) else {
        return GitInfo::not_git();
    };
    if !meta.is_dir() || fs::symlink_metadata(path.join(".git")).is_err() {
        return GitInfo::not_git();
    }

    let Some(rev_out) = run_git(path, &["rev-parse", "--is-inside-work-tree", "--git-dir"]) else {
        return GitInfo::not_git();
    };
    let mut lines = rev_out.lines();
    if lines.next().map(str::trim) != Some("true") {
        return GitInfo::not_git();
    }
    let git_dir_raw = lines.next().map(str::trim).unwrap_or(".git");
    let git_dir = {
        let p = PathBuf::from(git_dir_raw);
        if p.is_absolute() {
            p
        } else {
            path.join(p)
        }
    };

    if has_in_progress_op(&git_dir) {
        return GitInfo::dirty();
    }

    // Working tree + index must have no staged, unstaged, or untracked files.
    let Some(status_out) = run_git(path, &["status", "--porcelain", "--ignored=no"]) else {
        return GitInfo::dirty();
    };
    if !status_out.trim().is_empty() {
        return GitInfo::dirty();
    }

    // Stashes hold uncommitted work that would be lost on deletion.
    let Some(stash_out) = run_git(path, &["stash", "list"]) else {
        return GitInfo::dirty();
    };
    if !stash_out.trim().is_empty() {
        return GitInfo::dirty();
    }

    // Collect valid remotes (both fetch and push URLs must be valid remote URLs).
    let Some(remotes_out) = run_git(path, &["remote"]) else {
        return GitInfo::no_remote();
    };
    let mut valid_remotes: Vec<String> = Vec::new();
    for r in remotes_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let fetch_ok = run_git(path, &["remote", "get-url", r])
            .is_some_and(|u| is_valid_remote_url(&u));
        let push_ok = run_git(path, &["remote", "get-url", "--push", r])
            .is_some_and(|u| is_valid_remote_url(&u));
        if fetch_ok && push_ok {
            valid_remotes.push(r.to_string());
        }
    }
    if valid_remotes.is_empty() {
        return GitInfo::no_remote();
    }

    // Must be on a named branch (not detached HEAD) with at least one commit.
    let Some(branch_out) = run_git(path, &["symbolic-ref", "-q", "--short", "HEAD"]) else {
        return GitInfo::unpushed();
    };
    let branch = branch_out.trim();
    if branch.is_empty() || run_git(path, &["rev-parse", "--verify", "-q", "HEAD"]).is_none() {
        return GitInfo::unpushed();
    }

    // Current branch must have 0 commits ahead of its upstream (if configured)
    // or a matching remote-tracking branch on a valid remote.
    if let Some(up_out) = run_git(path, &["rev-parse", "--symbolic-full-name", "@{upstream}"]) {
        let up = up_out.trim();
        let up_remote = up
            .strip_prefix("refs/remotes/")
            .and_then(|s| s.split('/').next())
            .unwrap_or("");
        if !valid_remotes.iter().any(|r| r == up_remote) {
            return GitInfo::no_remote();
        }
        let range = format!("{up}..HEAD");
        let Some(ahead) = run_git(path, &["rev-list", "--count", &range]) else {
            return GitInfo::unpushed();
        };
        if ahead.trim() != "0" {
            return GitInfo::unpushed();
        }
    } else {
        let mut matched_remote_branch = false;
        for r in &valid_remotes {
            let remote_ref = format!("refs/remotes/{r}/{branch}");
            if run_git(path, &["rev-parse", "--verify", "-q", &remote_ref]).is_some() {
                let range = format!("{remote_ref}..HEAD");
                if run_git(path, &["rev-list", "--count", &range])
                    .is_some_and(|c| c.trim() == "0")
                {
                    matched_remote_branch = true;
                    break;
                }
            }
        }
        if !matched_remote_branch {
            return GitInfo::unpushed();
        }
    }

    // Every local branch must also have 0 commits not reachable from valid remotes.
    let mut rev_args: Vec<String> = vec![
        "rev-list".into(),
        "--count".into(),
        "--branches".into(),
        "--not".into(),
    ];
    for r in &valid_remotes {
        rev_args.push(format!("--remotes={r}"));
    }
    let rev_refs: Vec<&str> = rev_args.iter().map(String::as_str).collect();
    let Some(unpushed_all) = run_git(path, &rev_refs) else {
        return GitInfo::unpushed();
    };
    if unpushed_all.trim() != "0" {
        return GitInfo::unpushed();
    }

    GitInfo::clean_pushed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("git-test")
            .join(format!(
                "{}-{}-{}",
                name,
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed in {}", args, dir.display());
    }

    #[test]
    fn remote_url_validation() {
        assert!(is_valid_remote_url("https://github.com/lmctl-ai/lmclean.git"));
        assert!(is_valid_remote_url("git@github.com:lmctl-ai/lmclean.git"));
        assert!(is_valid_remote_url("ssh://git@github.com/lmctl-ai/lmclean.git"));
        assert!(is_valid_remote_url("file:///tmp/remote.git"));
        assert!(!is_valid_remote_url(""));
        assert!(!is_valid_remote_url("DISABLED-no-push-until-go-ahead"));
        assert!(!is_valid_remote_url("DISABLED-local-only"));
        assert!(!is_valid_remote_url("/srv/local/repo.git"));
        assert!(!is_valid_remote_url("./local/repo.git"));
        assert!(!is_valid_remote_url("C:\\repos\\foo.git"));
    }

    #[test]
    fn git_states_detected_conservatively() {
        let base = temp_dir("states");
        let plain = base.0.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(inspect(&plain), GitInfo::not_git());

        // Create a bare remote and a working repo.
        let remote = base.0.join("remote.git");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-b", "main"]);

        let work = base.0.join("work");
        fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-b", "main"]);
        fs::write(work.join("a.txt"), "hello\n").unwrap();

        // Untracked file -> dirty.
        assert_eq!(inspect(&work), GitInfo::dirty());

        git(&work, &["add", "a.txt"]);
        git(&work, &["commit", "-m", "initial"]);

        // Clean, but no remote configured -> no-remote.
        assert_eq!(inspect(&work), GitInfo::no_remote());

        // Local filesystem path remote (not file://) -> no-remote.
        git(&work, &["remote", "add", "origin", remote.to_str().unwrap()]);
        assert_eq!(inspect(&work), GitInfo::no_remote());

        // Switch remote to file:// URL, but not pushed yet -> unpushed.
        let file_url = format!("file://{}", remote.to_str().unwrap());
        git(&work, &["remote", "set-url", "origin", &file_url]);
        assert_eq!(inspect(&work), GitInfo::unpushed());

        // Push to origin/main with upstream tracking -> clean-pushed!
        git(&work, &["push", "-u", "origin", "main"]);
        assert_eq!(inspect(&work), GitInfo::clean_pushed());

        // Disabled push URL -> no-remote.
        git(&work, &["remote", "set-url", "--push", "origin", "DISABLED-no-push"]);
        assert_eq!(inspect(&work), GitInfo::no_remote());
        git(&work, &["remote", "set-url", "--push", "origin", &file_url]);
        assert_eq!(inspect(&work), GitInfo::clean_pushed());

        // Local commit ahead of origin/main -> unpushed.
        fs::write(work.join("a.txt"), "v2\n").unwrap();
        git(&work, &["commit", "-am", "v2"]);
        assert_eq!(inspect(&work), GitInfo::unpushed());

        // Push v2 -> clean-pushed again.
        git(&work, &["push", "origin", "main"]);
        assert_eq!(inspect(&work), GitInfo::clean_pushed());

        // Unpushed commit on a secondary branch while main is checked out -> unpushed.
        git(&work, &["checkout", "-b", "side"]);
        fs::write(work.join("side.txt"), "side\n").unwrap();
        git(&work, &["add", "side.txt"]);
        git(&work, &["commit", "-m", "side commit"]);
        git(&work, &["checkout", "main"]);
        assert_eq!(inspect(&work), GitInfo::unpushed());

        // Delete the unpushed side branch -> clean-pushed again.
        git(&work, &["branch", "-D", "side"]);
        assert_eq!(inspect(&work), GitInfo::clean_pushed());

        // Stashed change -> dirty.
        fs::write(work.join("a.txt"), "stashed\n").unwrap();
        git(&work, &["stash"]);
        assert_eq!(inspect(&work), GitInfo::dirty());
        git(&work, &["stash", "drop"]);
        assert_eq!(inspect(&work), GitInfo::clean_pushed());
    }
}
