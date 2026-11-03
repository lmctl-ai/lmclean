//! fd-style recursive walking: deterministic (sorted), hidden-skipping by default,
//! depth-limited on request, symlinks never followed, unreadable dirs skipped.
use dslvm::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Options {
    pub hidden: bool,
    pub max_depth: Option<u32>,
}

/// One visited entry. `metadata` comes from `symlink_metadata`, so a symlink is
/// reported as a link and never counts as the directory it points at.
pub struct Candidate {
    pub path: PathBuf,
    pub depth: u32,
    pub metadata: fs::Metadata,
}

/// Raw bytes of a path or file name (Unix paths are byte strings).
#[cfg(unix)]
pub fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(s).to_vec()
}
#[cfg(not(unix))]
pub fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

pub fn path_bytes(path: &Path) -> Vec<u8> {
    os_bytes(path.as_os_str())
}

#[cfg(unix)]
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}
#[cfg(not(unix))]
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    os_bytes(name).first() == Some(&b'.')
}

pub fn modified_secs(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn accessed_secs(metadata: &fs::Metadata) -> i64 {
    let mtime = modified_secs(metadata);
    metadata
        .accessed()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(mtime)
}

/// Recursive `(max_accessed, max_modified)` timestamps in Unix seconds across
/// a candidate subtree. For a non-directory entry, returns its own `(accessed,
/// modified)`. For a directory, walks all non-directory descendants (never
/// following symlinks), ignoring `.git` internal metadata unless the directory
/// contains no non-`.git` files; for an empty directory, falls back to the
/// directory's own `(accessed, modified)`.
pub fn rtimes(path: &Path) -> (i64, i64) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    let own_a = accessed_secs(&metadata);
    let own_m = modified_secs(&metadata);
    if !metadata.is_dir() {
        return (own_a, own_m);
    }
    let mut work_a: Option<i64> = None;
    let mut work_m: Option<i64> = None;
    let mut vcs_a: Option<i64> = None;
    let mut vcs_m: Option<i64> = None;
    rtimes_walk(path, false, &mut work_a, &mut work_m, &mut vcs_a, &mut vcs_m);
    match (work_a, work_m, vcs_a, vcs_m) {
        (Some(a), Some(m), _, _) => (a, m),
        (_, _, Some(a), Some(m)) => (a, m),
        _ => (own_a, own_m),
    }
}

fn rtimes_walk(
    dir: &Path,
    in_vcs: bool,
    work_a: &mut Option<i64>,
    work_m: &mut Option<i64>,
    vcs_a: &mut Option<i64>,
    vcs_m: &mut Option<i64>,
) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_git = in_vcs || entry.file_name() == ".git";
        if meta.is_dir() {
            rtimes_walk(&entry.path(), is_git, work_a, work_m, vcs_a, vcs_m);
        } else {
            let a = accessed_secs(&meta);
            let m = modified_secs(&meta);
            if is_git {
                *vcs_a = Some(vcs_a.map_or(a, |prev| prev.max(a)));
                *vcs_m = Some(vcs_m.map_or(m, |prev| prev.max(m)));
            } else {
                *work_a = Some(work_a.map_or(a, |prev| prev.max(a)));
                *work_m = Some(work_m.map_or(m, |prev| prev.max(m)));
            }
        }
    }
}

/// Recursive size in bytes, self-contained (std::fs only, no external du/df):
/// the entry itself plus, for directories, all descendants recursively,
/// counting only non-directory entries. Symlinks contribute their own
/// `symlink_metadata` len and are never followed; directory inode sizes are
/// excluded, so results are filesystem-independent (unlike `du -b`). Hard
/// links are counted per link (no inode dedup). Unreadable subtrees are
/// skipped. For a non-directory entry this equals its own len.
pub fn rsize(path: &Path) -> i64 {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if !metadata.is_dir() {
        return metadata.len() as i64;
    }
    let read = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut total = 0i64;
    for entry in read.flatten() {
        // DirEntry::metadata does not follow symlinks.
        match entry.metadata() {
            Ok(m) if m.is_dir() => total = total.saturating_add(rsize(&entry.path())),
            Ok(m) => total = total.saturating_add(m.len() as i64),
            Err(_) => {}
        }
    }
    total
}

/// Removes a file, symlink, or directory tree. If standard removal fails with
/// `PermissionDenied` (for example, Go `GOMODCACHE` `0555` module directories
/// on Unix or read-only build artifacts on Windows), makes user-owned entries
/// inside `path` writable without following symlinks and retries once.
pub fn remove_entry(path: &Path, is_dir: bool) -> std::io::Result<()> {
    let attempt = || {
        if is_dir {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
    };
    match attempt() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable_tree(path);
            attempt()
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn make_dir_writable(dir: &Path, meta: &fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
    }
}

#[cfg(not(unix))]
fn make_dir_writable(dir: &Path, meta: &fs::Metadata) {
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = fs::set_permissions(dir, perms);
    }
}

fn make_writable_tree(path: &Path) {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        make_dir_writable(path, &meta);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable_tree(&entry.path());
            }
        }
    } else {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

/// Visits every candidate under `root` (or `root` itself when it is not a
/// directory, at depth 0). A directory's direct children have depth 0. The
/// callback's return value decides descent into a directory: `false` skips it
/// entirely (matched directories are never re-filtered inside).
pub fn walk(
    root: &Path,
    opts: &Options,
    visit: &mut dyn FnMut(&Candidate) -> Result<bool, Error>,
) -> Result<(), Error> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lmclean: cannot stat {}: {e}", root.display());
            return Ok(());
        }
    };
    if !metadata.is_dir() {
        let cand = Candidate {
            path: root.to_path_buf(),
            depth: 0,
            metadata,
        };
        visit(&cand)?;
        return Ok(());
    }
    walk_dir(root, 0, opts, visit)
}

fn walk_dir(
    dir: &Path,
    child_depth: u32,
    opts: &Options,
    visit: &mut dyn FnMut(&Candidate) -> Result<bool, Error>,
) -> Result<(), Error> {
    if let Some(max) = opts.max_depth {
        if child_depth > max {
            return Ok(());
        }
    }
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lmclean: cannot read {}: {e}", dir.display());
            return Ok(());
        }
    };
    let mut entries: Vec<fs::DirEntry> = Vec::new();
    for entry in read {
        match entry {
            Ok(e) => entries.push(e),
            Err(e) => eprintln!("lmclean: cannot read an entry in {}: {e}", dir.display()),
        }
    }
    entries.sort_by_key(|e| os_bytes(&e.file_name()));
    for entry in entries {
        if !opts.hidden && is_hidden(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("lmclean: cannot stat {}: {e}", path.display());
                continue;
            }
        };
        let cand = Candidate {
            path,
            depth: child_depth,
            metadata,
        };
        let descend = visit(&cand)?;
        if cand.metadata.is_dir() && descend {
            walk_dir(&cand.path, child_depth + 1, opts, visit)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("walk-test")
            .join(format!("{}-{}", name, SEQ.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collect(root: &Path, opts: &Options) -> Vec<(String, u32)> {
        let mut seen = Vec::new();
        walk(root, opts, &mut |cand| {
            seen.push((
                cand.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                cand.depth,
            ));
            Ok(true)
        })
        .unwrap();
        seen
    }

    #[test]
    fn sorted_deterministic_order() {
        let dir = scratch("sorted");
        for name in ["b.txt", "a.txt", "c.txt"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::create_dir(dir.join("aa")).unwrap();
        fs::write(dir.join("aa").join("z.txt"), b"x").unwrap();
        let seen = collect(&dir, &Options::default());
        let names: Vec<_> = seen.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a.txt", "aa", "aa/z.txt", "b.txt", "c.txt"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_skipped_by_default() {
        let dir = scratch("hidden");
        fs::write(dir.join("vis.txt"), b"x").unwrap();
        fs::write(dir.join(".hid.txt"), b"x").unwrap();
        fs::create_dir(dir.join(".hidir")).unwrap();
        fs::write(dir.join(".hidir").join("in.txt"), b"x").unwrap();
        let seen = collect(&dir, &Options::default());
        assert_eq!(seen.len(), 1);
        let seen = collect(
            &dir,
            &Options {
                hidden: true,
                max_depth: None,
            },
        );
        assert_eq!(seen.len(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_depth_limits_recursion() {
        let dir = scratch("depth");
        fs::create_dir_all(dir.join("d1").join("d2")).unwrap();
        fs::write(dir.join("top.txt"), b"x").unwrap();
        fs::write(dir.join("d1").join("mid.txt"), b"x").unwrap();
        fs::write(dir.join("d1").join("d2").join("deep.txt"), b"x")
            .unwrap();
        let opts = Options {
            hidden: false,
            max_depth: Some(0),
        };
        let seen = collect(&dir, &opts);
        let names: Vec<_> = seen.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["d1", "top.txt"]);
        assert!(seen.iter().all(|(_, d)| *d == 0));
        let opts = Options {
            hidden: false,
            max_depth: Some(1),
        };
        let seen = collect(&dir, &opts);
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|(_, d)| *d <= 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_reported_not_followed() {
        let dir = scratch("symlink");
        fs::create_dir(dir.join("real")).unwrap();
        fs::write(dir.join("real").join("inside.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let mut links = 0;
        let mut visited: Vec<String> = Vec::new();
        walk(&dir, &Options::default(), &mut |cand| {
            if cand.metadata.is_symlink() {
                links += 1;
            }
            visited.push(
                cand.path
                    .strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            Ok(true)
        })
        .unwrap();
        assert_eq!(links, 1);
        assert!(visited.contains(&"link".to_string()));
        assert!(!visited.contains(&"link/inside.txt".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn declined_dir_is_not_descended() {
        let dir = scratch("nodescend");
        fs::create_dir(dir.join("skipme")).unwrap();
        fs::write(dir.join("skipme").join("inner.txt"), b"x").unwrap();
        let mut visited: Vec<String> = Vec::new();
        walk(&dir, &Options::default(), &mut |cand| {
            let rel = cand
                .path
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let descend = rel != "skipme";
            visited.push(rel);
            Ok(descend)
        })
        .unwrap();
        assert!(visited.contains(&"skipme".to_string()));
        assert!(!visited.contains(&"skipme/inner.txt".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rsize_sums_non_dir_entries_exactly() {
        let dir = scratch("rsize");
        fs::create_dir_all(dir.join("sub").join("deep")).unwrap();
        fs::write(dir.join("a.txt"), [0u8; 10]).unwrap();
        fs::write(dir.join("sub").join("b.txt"), [0u8; 20]).unwrap();
        fs::write(dir.join("sub").join("deep").join("c.txt"), [0u8; 30]).unwrap();
        // Directory inode sizes are excluded: exactly the three file lens.
        assert_eq!(rsize(&dir), 60);
        assert_eq!(rsize(&dir.join("sub")), 50);
        // For a non-directory entry rsize equals its own len.
        assert_eq!(rsize(&dir.join("a.txt")), 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rsize_counts_symlink_own_len_not_target() {
        let dir = scratch("rsize-link");
        fs::write(dir.join("target.txt"), [0u8; 100]).unwrap();
        // Relative target keeps the link's own len deterministic: 10 bytes.
        std::os::unix::fs::symlink("target.txt", dir.join("the-link")).unwrap();
        let link_len = fs::symlink_metadata(dir.join("the-link")).unwrap().len() as i64;
        assert_eq!(link_len, 10);
        assert_eq!(rsize(&dir.join("the-link")), link_len);
        assert_eq!(rsize(&dir), 100 + link_len);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rsize_skips_unreadable_subtrees() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("rsize-unreadable");
        fs::create_dir(dir.join("secret")).unwrap();
        fs::write(dir.join("open.txt"), [0u8; 7]).unwrap();
        fs::write(dir.join("secret").join("hidden.txt"), [0u8; 500]).unwrap();
        fs::set_permissions(dir.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(rsize(&dir), 7);
        fs::set_permissions(dir.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn remove_entry_cleans_readonly_subdirs_without_following_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("remove-readonly");
        let outside = dir.join("outside");
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("keep.txt");
        fs::write(&keep, b"survive").unwrap();
        fs::set_permissions(&keep, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o555)).unwrap();

        let gomod = dir.join("gomodcache");
        let mod_dir = gomod.join("example.com").join("pkg@v1.0.0");
        fs::create_dir_all(&mod_dir).unwrap();
        let mod_file = mod_dir.join("lib.go");
        fs::write(&mod_file, b"package pkg").unwrap();
        std::os::unix::fs::symlink(&outside, mod_dir.join("ext-link")).unwrap();
        fs::set_permissions(&mod_file, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&mod_dir, fs::Permissions::from_mode(0o555)).unwrap();

        remove_entry(&gomod, true).unwrap();
        assert!(!gomod.exists());
        assert!(keep.exists(), "symlink target must not be removed or mutated");
        assert_eq!(
            fs::symlink_metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o555
        );

        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&keep, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
