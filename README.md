# lmclean

Scriptable file search & cleanup in the lmcode ecosystem. Rules are lmcode
programs (`.lms` with a `_lmcode_` block) compiled externally by lmscript into
verified bytecode v6 (`.dslb`); lmclean is the native host that loads that
bytecode, walks the filesystem fd-style, calls each `@select` rule once per
candidate file, and lets the boolean result decide what gets deleted.

## Safety contract

- **Dry-run is the default.** Without `--exec` nothing is ever removed; stdout
  lists what *would* be deleted.
- `--exec` deletes (unlink for files/symlinks, recursive removal for matched
  directories).
- `--exec --trash DIR` moves matches into `DIR` instead, preserving each path
  relative to its search root, and appends every move to `DIR/manifest.jsonl`.
- `--trash DIR --restore` replays the manifest in reverse order and moves every
  entry back to its original path.
- Symlinks are never followed: a link is reported (`is_symlink()`) but never
  descended.
- A matched directory is never re-filtered inside: once a directory matches,
  the walk does not descend into it.
- Hidden files/dirs (dot names) are skipped unless `--hidden` is given.
- Unreadable directories are noted on stderr and skipped, never fatal.

## Usage

```
lmclean --version
lmclean --lmcode-schema [--json]      # exact schema bytes (hash-bound to bytecode)
lmclean --lmcode-signatures           # canonical registry signatures
lmclean --lmcode FILE.dslb [OPTIONS] [PATH ...]     # PATH default: "."
  --exec          actually delete (default: dry-run)
  --trash DIR     move matches into DIR + manifest.jsonl; requires --exec
  --restore       with --trash DIR: move manifest entries back, then exit
  --hidden        include hidden files/dirs
  --max-depth N   limit recursion depth (a root's direct children are depth 0)
```

Exit codes (R42 host protocol): `0` ok, `2` CLI usage error, `3` load/run
error, `4` application failure (e.g. unreadable bytecode file, failed
deletions).

### stdout protocol (stable, for scripting)

One JSON line per event; stderr carries `print()` output and diagnostics only.

- Dry-run match: `{"plan":"delete","path":"...","size":N}`
- Exec match: `{"deleted":"unlink","path":"...","size":N}` or
  `{"deleted":"trash","stored":"...","path":"...","size":N}`
- Summary: `{"ok":true,"matched":N,"bytes":B,"dry_run":true|false}`
- Restore: `{"restored":"...","from":"..."}` per file, then
  `{"ok":true,"restored":N}`
- Errors: `{"ok": false, "phase": "cli|load|run|app", "code": ..., "message": ...}`

In match and summary lines, `"size"`/`"bytes"` carry the **recursive size**
(see `rsize` below), computed once per match — never for non-matches — so
dry-run plans show real reclaimable bytes.

Paths are byte strings; JSON escaping is byte-exact (`\"`, `\\`, `\n`, `\r`,
`\t`, `\u00XX` for other control bytes, everything else verbatim).

## Sizes: `size()` vs `rsize()`

`size()` is the entry's own `symlink_metadata` length. `rsize()` is the
recursive size, computed natively with std::fs only — lmclean never shells out
to `du`/`df` or any external tool:

- `rsize` = the entry itself plus, for directories, all descendants
  recursively, **counting only non-directory entries**. For a non-directory
  entry, `rsize() == size()`.
- Directory inode sizes are excluded, so results are filesystem-independent
  and deterministic (unlike `du -b`, which counts them).
- Symlinks contribute their own link length (target size is never followed or
  counted).
- Hard links are counted per link — there is no inode dedup (unlike `du`,
  which counts each inode once).
- Unreadable subtrees are skipped (best effort, no abort).

## Authoring rules

A rule file is an lmscript source with one `_lmcode_` block against
`clean.dsl.json`. `@select` entries implement the SAM interface
`Filter(File) -> bool` (the deletion predicate); `@report` entries implement
`Report(File) -> void` and run once per matched file (logging/auditing, via
`print()`, which writes to stderr). Multiple `@select` entries are OR-ed.

lmclean has **no .gitignore support and no built-in ignore rules**: pruning is
the program's job. `@ignore` entries implement `Ignore(File) -> bool` and are
evaluated for every candidate **before** any `@select` entry. If any `@ignore`
entry returns true, the candidate is skipped entirely — not filtered, not
listed — and if it is a directory the walker does not descend into it. A
program with no `@ignore` entries ignores nothing (subject only to the
explicit `--hidden` / `--max-depth` CLI flags).

```text
program = _lmcode_ schema="../clean.dsl.json";
@ignore
fn skip_vcs(f: File) -> bool implements Ignore {
  return f.is_dir() && f.name() == ".git";
}
@select
fn stale_junk(f: File) -> bool implements Filter {
  let ext = f.extension();
  let stale = now() - f.modified() > 7 * 24 * 3600;
  return f.is_file() && stale && (ext == "tmp" || ext == "log");
}
end _lmcode_
savedsl(program, ARGV[0]);
```

Compile with the mono lmscript build (see durable-memory/03-build-run-test.md):

```
/home/ubuntu/lmctl-mono/lmscript/bin/lmscript examples/clean-tmp.lms clean-tmp.dslb
lmclean --lmcode clean-tmp.dslb --exec /var/tmp/myapp
```

`File` methods: `path()`, `name()`, `extension()` (no dot, empty if none),
`size()` (own length), `rsize()` (recursive size, see above), `depth()`
(relative to its search root; direct children = 0),
`is_file()`, `is_dir()`, `is_symlink()`, `modified()` (mtime epoch seconds).
Host functions: `print(string)` (stderr), `now()` (epoch seconds, for
age-based rules). The implicit 72-signature lmcode std catalog
(`str_*`, `math_*`, collections, JSON, ...) is also available to rules.

The schema bytes are SHA-256-bound into every compiled program: any change to
`clean.dsl.json` (even whitespace) breaks loading of previously compiled
`.dslb` files.

## Layout

- `src/main.rs` — CLI, R42 flags, load/run/dispatch, exit protocol
- `src/host.rs` — `File` type, registry wiring, arena handles, `print`/`now`
- `src/walk.rs` — std::fs recursive walker (sorted, hidden-skip, depth limit)
- `src/trash.rs` — trash moves with manifest, restore
- `examples/` — `clean-tmp.lms`, `clean-build-dirs.lms`, `clean-ignore.lms`,
  `clean-stale-build-dirs.lms`
- `test.sh` — the gate: unit tests, release build, example compilation,
  end-to-end dry-run/exec/trash/restore/exit-code/hidden checks

Run the gate with `bash test.sh`.
