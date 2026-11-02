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

## Server mode (`--serve`)

A small REST server around the same rule engine, for clients that want to scan
repeatedly, review, and then clean without re-walking. std only, one connection
at a time, JSON in/out, `Connection: close`.

```
lmclean --serve [--listen ADDR:PORT]   # default 127.0.0.1:0 (ephemeral port)
                [--presets FILE]       # named rule/settings bundles, see presets.example.json
                [--preset NAME]        # start on this preset (default: the file's "default")
                [--idle-timeout SECS]  # exit after SECS without a request; default 300, 0 = never
                [--allow-exec]         # permit POST /clean and /restore (default: scan-only)
                [--token TOKEN]        # require "Authorization: Bearer TOKEN"
```

On startup stdout gets `{"listening":"127.0.0.1:PORT"}` (scriptable even with
port 0); on exit `{"shutdown":"requested"}` or `{"shutdown":"idle"}`, exit code
0. A non-loopback `--listen` requires `--token`. Exit codes: `2` usage, `3`
start-preset bytecode fails to load, `4` unreadable/invalid presets file.

| Request | Effect |
|---|---|
| `GET /health` | liveness, version, `cached`, `allow_exec`, `idle_timeout` |
| `GET /presets` | the loaded presets, default and active name |
| `GET /settings` | current `program`, `paths`, `hidden`, `max_depth`, `trash` |
| `PUT /settings` (or `PATCH`) | partial update; `{"preset":"name"}` loads a preset first, other keys override it. The program is loaded and verified immediately (400 on failure, nothing changes). Any change drops the cache. Editing without naming a preset makes `preset` `null` (custom) |
| `POST /scan` | walk and **cache** the matches (never deletes): `{"matched","bytes","scanned_at","matches":[{"path","size","kind"}]}` |
| `GET /results` | the cached scan, no walking (`"cached":true`); 404 if none |
| `POST /clean` | delete (or move to `trash`) exactly the cached matches, then drop the cache. 403 without `--allow-exec`, 409 without a cached scan |
| `POST /restore` | replay the trash manifest (`settings.trash`, or `{"trash":"DIR"}` in the body); needs `--allow-exec` |
| `POST /shutdown` | reply, then exit |

Dry-run stays the default over the wire: without `--allow-exec` the server can
only scan. Matched sizes are the same recursive `rsize` as the CLI.

```
lmclean --serve --presets presets.example.json --idle-timeout 120 --allow-exec &
curl -s localhost:$PORT/scan -X POST          # review the plan
curl -s localhost:$PORT/results               # same result, from cache
curl -s localhost:$PORT/settings -X PUT -d '{"preset":"build-dirs"}'
curl -s localhost:$PORT/clean -X POST         # act on the cached scan
curl -s localhost:$PORT/shutdown -X POST
```

A presets file is `{"default":"name","presets":{"name":{"program":"x.dslb",
"paths":[...],"hidden":false,"max_depth":null,"trash":null}}}`; relative paths
inside it resolve against the file's directory.

Integration tests: `cargo test --test server` (runs everywhere, no bytecode
needed for the protocol tests). The per-example tests need compiled bytecode in
`tests/fixtures/` (see its README); CI requires it.

## Layout

- `src/main.rs` — CLI, R42 flags, load/run/dispatch, exit protocol
- `src/host.rs` — `File` type, registry wiring, arena handles, `print`/`now`
- `src/walk.rs` — std::fs recursive walker (sorted, hidden-skip, depth limit)
- `src/trash.rs` — trash moves with manifest, restore
- `src/server.rs`, `src/json.rs` — `--serve` REST mode and its minimal JSON reader
- `examples/` — `clean-tmp.lms`, `clean-build-dirs.lms`, `clean-ignore.lms`,
  `clean-stale-build-dirs.lms`
- `tests/server.rs` — REST integration tests (protocol + one per example)
- `test.sh` — the gate: unit tests, release build, example compilation,
  end-to-end dry-run/exec/trash/restore/exit-code/hidden checks

Run the gate with `bash test.sh`.
