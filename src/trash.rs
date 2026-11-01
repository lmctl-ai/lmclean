//! Trash moves and manifest-driven restore. Every stored match is appended to
//! `DIR/manifest.jsonl` as `{"stored":"...","original":"..."}`; restore replays
//! the manifest in reverse order so suffix collisions undo cleanly.
use crate::host::write_json_string;
use crate::walk::{path_bytes, path_from_bytes};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const EXDEV: i32 = 18; // cross-device link: rename cannot move across filesystems

pub struct Trash {
    manifest: fs::File,
    dir: PathBuf,
}

impl Trash {
    pub fn open(dir: &Path) -> io::Result<Trash> {
        fs::create_dir_all(dir)?;
        let manifest = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("manifest.jsonl"))?;
        Ok(Trash {
            manifest,
            dir: dir.to_path_buf(),
        })
    }

    /// Moves `original` (walked under `root`) into the trash dir, preserving its
    /// path relative to the root. Collisions get a numeric suffix, recorded in
    /// the manifest. Returns the stored path.
    pub fn store(&mut self, original: &Path, root: &Path) -> io::Result<PathBuf> {
        let rel = match original.strip_prefix(root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
            _ => PathBuf::from(original.file_name().unwrap_or_default()),
        };
        let mut dest = self.dir.join(&rel);
        for n in 1u32.. {
            if fs::symlink_metadata(&dest).is_err() {
                break;
            }
            dest = self.dir.join(format!("{}.{n}", rel.to_string_lossy()));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        move_path(original, &dest)?;
        let mut line = Vec::new();
        line.extend_from_slice(b"{\"stored\":");
        write_json_string(&mut line, &path_bytes(&dest))?;
        line.extend_from_slice(b",\"original\":");
        write_json_string(&mut line, &path_bytes(original))?;
        line.extend_from_slice(b"}\n");
        self.manifest.write_all(&line)?;
        self.manifest.flush()?;
        Ok(dest)
    }
}

fn is_real_dir(path: &Path) -> io::Result<bool> {
    Ok(fs::symlink_metadata(path)?.is_dir())
}

/// rename, with a copy-and-remove fallback when the trash dir lives on another
/// filesystem.
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            copy_tree(from, to)?;
            if is_real_dir(from)? {
                fs::remove_dir_all(from)
            } else {
                fs::remove_file(from)
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(not(unix))]
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// Moves every manifest entry back to its original path, newest entries first.
/// Writes one `{"restored":"...","from":"..."}` line per recovered match.
pub fn restore(dir: &Path, out: &mut impl Write) -> io::Result<u64> {
    let manifest = dir.join("manifest.jsonl");
    let data = fs::read(&manifest)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", manifest.display())))?;
    let lines: Vec<&[u8]> = data
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let mut restored = 0u64;
    for line in lines.iter().rev() {
        let Some((stored, original)) = parse_manifest_line(line) else {
            eprintln!("lmclean: restore: skipping malformed manifest line");
            continue;
        };
        let stored_path = path_from_bytes(&stored);
        let original_path = path_from_bytes(&original);
        if fs::symlink_metadata(&stored_path).is_err() {
            eprintln!(
                "lmclean: restore: stored path is gone: {}",
                stored_path.display()
            );
            continue;
        }
        if let Some(parent) = original_path.parent() {
            fs::create_dir_all(parent)?;
        }
        move_path(&stored_path, &original_path)?;
        out.write_all(b"{\"restored\":")?;
        write_json_string(out, &original)?;
        out.write_all(b",\"from\":")?;
        write_json_string(out, &stored)?;
        out.write_all(b"}\n")?;
        restored += 1;
    }
    Ok(restored)
}

/// Parses one manifest line exactly as `store` writes it.
fn parse_manifest_line(line: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let rest = line.strip_prefix(b"{\"stored\":")?;
    let (stored, rest) = parse_json_string(rest)?;
    let rest = rest.strip_prefix(b",\"original\":")?;
    let (original, rest) = parse_json_string(rest)?;
    if rest != b"}" {
        return None;
    }
    Some((stored, original))
}

fn parse_json_string(b: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut b = b.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    loop {
        let (&c, rest) = b.split_first()?;
        b = rest;
        match c {
            b'"' => return Some((out, b)),
            b'\\' => {
                let (&e, rest) = b.split_first()?;
                b = rest;
                match e {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        if b.len() < 4 {
                            return None;
                        }
                        let hex = std::str::from_utf8(&b[..4]).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        b = &b[4..];
                        if cp < 0x80 {
                            out.push(cp as u8);
                        } else {
                            let ch = char::from_u32(cp)?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    _ => return None,
                }
            }
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("trash-test")
            .join(format!("{}-{}", name, SEQ.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn store_restore_round_trip() {
        let dir = scratch("round-trip");
        let root = dir.join("root");
        let trash_dir = dir.join("trash");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("a.tmp"), b"alpha").unwrap();
        fs::write(root.join("b.tmp"), b"beta").unwrap();
        let original_a = fs::read(root.join("sub").join("a.tmp")).unwrap();
        let original_b = fs::read(root.join("b.tmp")).unwrap();

        let mut trash = Trash::open(&trash_dir).unwrap();
        let stored_a = trash.store(&root.join("sub").join("a.tmp"), &root).unwrap();
        let stored_b = trash.store(&root.join("b.tmp"), &root).unwrap();
        drop(trash);
        assert_eq!(stored_a, trash_dir.join("sub").join("a.tmp"));
        assert_eq!(stored_b, trash_dir.join("b.tmp"));
        assert!(!root.join("sub").join("a.tmp").exists());
        assert!(!root.join("b.tmp").exists());

        let manifest = fs::read(trash_dir.join("manifest.jsonl")).unwrap();
        let text = String::from_utf8(manifest).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"stored\":"));
        assert!(text.contains("\"original\":"));

        let mut out = Vec::new();
        let restored = restore(&trash_dir, &mut out).unwrap();
        assert_eq!(restored, 2);
        assert_eq!(fs::read(root.join("sub").join("a.tmp")).unwrap(), original_a);
        assert_eq!(fs::read(root.join("b.tmp")).unwrap(), original_b);
        assert!(!stored_a.exists());
        assert!(!stored_b.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collisions_get_numeric_suffix() {
        let dir = scratch("collision");
        let root_a = dir.join("root-a");
        let root_b = dir.join("root-b");
        let trash_dir = dir.join("trash");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_a.join("same.tmp"), b"one").unwrap();
        fs::write(root_b.join("same.tmp"), b"two").unwrap();

        let mut trash = Trash::open(&trash_dir).unwrap();
        let first = trash.store(&root_a.join("same.tmp"), &root_a).unwrap();
        let second = trash.store(&root_b.join("same.tmp"), &root_b).unwrap();
        drop(trash);
        assert_eq!(first, trash_dir.join("same.tmp"));
        assert_eq!(second, trash_dir.join("same.tmp.1"));

        let mut out = Vec::new();
        let restored = restore(&trash_dir, &mut out).unwrap();
        assert_eq!(restored, 2);
        assert_eq!(fs::read(root_a.join("same.tmp")).unwrap(), b"one");
        assert_eq!(fs::read(root_b.join("same.tmp")).unwrap(), b"two");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_is_moved_and_restored_whole() {
        let dir = scratch("dir");
        let root = dir.join("root");
        let trash_dir = dir.join("trash");
        fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
        fs::write(root.join("node_modules").join("pkg").join("i.js"), b"m").unwrap();

        let mut trash = Trash::open(&trash_dir).unwrap();
        trash.store(&root.join("node_modules"), &root).unwrap();
        drop(trash);
        assert!(!root.join("node_modules").exists());
        assert!(trash_dir.join("node_modules").join("pkg").join("i.js").exists());

        let mut out = Vec::new();
        assert_eq!(restore(&trash_dir, &mut out).unwrap(), 1);
        assert_eq!(
            fs::read(root.join("node_modules").join("pkg").join("i.js")).unwrap(),
            b"m"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Windows forbids '"' in filenames, so the escape fixture is unix-only.
    #[cfg(unix)]
    #[test]
    fn manifest_line_parsing_handles_escapes() {
        let dir = scratch("escapes");
        let root = dir.join("root");
        let trash_dir = dir.join("trash");
        fs::create_dir_all(&root).unwrap();
        let name = "weird \"name\"\t.tmp";
        fs::write(root.join(name), b"z").unwrap();
        let mut trash = Trash::open(&trash_dir).unwrap();
        trash.store(&root.join(name), &root).unwrap();
        drop(trash);
        let mut out = Vec::new();
        assert_eq!(restore(&trash_dir, &mut out).unwrap(), 1);
        assert_eq!(fs::read(root.join(name)).unwrap(), b"z");
        let _ = fs::remove_dir_all(&dir);
    }
}
