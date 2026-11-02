# lmclean

Scriptable file search & cleanup in the lmcode ecosystem. Rules are lmcode
programs (`.lms` with a `_lmcode_` block) that lmscript compiles into verified
bytecode v6 and hands to lmclean through `run_lmcode`; lmclean is the native
host that loads that bytecode, walks the filesystem fd-style, calls each `@select` rule once per
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
lmclean --lmcode FILE.dslb [OPTIONS] [PATH ...]     # PATH default: "." (FILE is supplied by run_lmcode)
  --exec          actually delete (default: dry-run)
  --trash DIR     move matches into DIR + manifest.jsonl; requires --exec
  --restore       with --trash DIR: move manifest entries back, then exit
  --hidden        include hidden files/dirs
  --max-depth N   limit recursion depth (a root's direct children are depth 0)
  --param N=V     rule parameter, read by rules via param_int/param_str (repeatable)
lmclean --serve [...]                  # REST server, see "Server mode"
```

Exit codes (R42 host protocol): `0` ok, `2` CLI usage error, `3` load/run
error, `4` application failure (e.g. unreadable bytecode file, failed
deletions).

### stdout protocol (stable, for scripting)

One JSON line per event, then one terminal return frame. stderr carries
`print()` output and diagnostics only.

- Dry-run match: `{"plan":"delete","path":"...","size":N}`
- Exec match: `{"deleted":"unlink","path":"...","size":N}` or
  `{"deleted":"trash","stored":"...","path":"...","size":N}`
- Restore: `{"restored":"...","from":"..."}` per file
- Terminal frame (always last, success and failure alike):

  ```
  LMCODE_RETURN_BEGIN
  {"ok":true,"result":{"matched":N,"bytes":B,"dry_run":true|false}}
  LMCODE_RETURN_END
  ```

  restore's result is `{"restored":N}`; failures are
  `{"ok":false,"phase":"cli|load|run|app","code":...,"message":...}`. This is
  the envelope `run_lmcode` consumes; the JSON inside is compact.

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
// ...followed by the run_lmcode driver lines shown in examples/*.lms
```

A rule file is also a **driver**: after the `_lmcode_` block it calls
`run_lmcode`, which compiles the program, writes it to a private temporary
`$DSLFILE` and launches lmclean on it, forwarding everything after the host
path (see the end of any file in `examples/`). There is no standalone bytecode
export: run the `.lms` with a current [lmscript](https://www.npmjs.com/package/@lmctl-ai/lmctl)
(`npm install @lmctl-ai/lmctl`).

```
lmscript examples/clean-tmp.lms target/release/lmclean /var/tmp/myapp                 # dry-run
lmscript examples/clean-tmp.lms target/release/lmclean --exec --param stale_days=3 /var/tmp/myapp
```

Rules read client-tunable parameters with `param_int(name, default)` and
`param_str(name, default)`; set them with `--param NAME=VALUE` or, in server
mode, the `params` setting. The bundled packs document theirs
(`stale_days`, `exts`, `names`, `ignore`, `stale_hours`).

`File` methods: `path()`, `name()`, `extension()` (no dot, empty if none),
`size()` (own length), `rsize()` (recursive size, see above), `depth()`
(relative to its search root; direct children = 0),
`is_file()`, `is_dir()`, `is_symlink()`, `modified()` (mtime epoch seconds).
Host functions: `print(string)` (stderr), `now()` (epoch seconds, for
age-based rules), `param_int(string, int) -> int`, `param_str(string, string) -> string`. The implicit 72-signature lmcode std catalog
(`str_*`, `math_*`, collections, JSON, ...) is also available to rules.

The schema bytes are SHA-256-bound into every compiled program: any change to
`clean.dsl.json` (even whitespace) breaks loading of previously compiled
bytecode. Programs are recompiled on every run, so this only matters for
bytecode kept on disk.

## Server mode (`--serve`)

A REST server (no GUI) around the same rule engine, built so a client can scan
once, then slice the result every way it likes, preview a delete, and only then
delete. std only, one connection at a time, JSON in/out, `Connection: close`.

```
lmscript examples/clean-tmp.lms target/release/lmclean --serve [SERVER OPTIONS]
lmscript examples/serve-all.lms target/release/lmclean --serve --presets examples/serve-presets.json
```

Rules reach the server through `run_lmcode` like everywhere else: the host gets
one `--lmcode FILE` per program (a script with one program, or a `lmcode_build`
builder with several: *slots* 0, 1, ...). A lone program is selected
automatically; with several, presets (or `PUT /settings {"program": N}`) pick
one. `run_lmcode` only relays host stdout when the host exits, so use
`--port-file FILE` to learn the address.

```
--listen ADDR:PORT   default 127.0.0.1:0 (ephemeral port)
--port-file FILE     write the bound address here once listening
--presets FILE       named settings bundles (see examples/serve-presets.json)
--preset NAME        start on this preset (default: the file's "default")
--idle-timeout SECS  exit after SECS without a request; default 300, 0 = never
--allow-exec         permit real deletes (POST /clean without dry_run, /restore)
--token TOKEN        require "Authorization: Bearer TOKEN"
--lmcode FILE        program slot (supplied by run_lmcode; repeatable)
```

The server exits on `POST /shutdown` or after the idle timeout, with exit code 0
and a terminal frame (`{"shutdown":"requested|idle"}`; one result per program
for a builder). A non-loopback `--listen` requires `--token`. Exit codes: `2`
usage, `3` a program fails to load, `4` bad presets file / unreadable input.

### Endpoints

| Request | Effect |
|---|---|
| `GET /health` | liveness, version, `cached`, `allow_exec`, `idle_timeout` |
| `GET /presets` | loaded presets, default and active name |
| `GET /settings` | `program`, `paths`, `hidden`, `max_depth`, `trash`, `params` |
| `PUT /settings` (or `PATCH`) | partial update. `{"preset":"x"}` loads a preset first, other keys override. `program` is a slot number or a bytecode path and is verified immediately (400, nothing changes). `params` merges (`null` removes a key). Any change drops the cache; editing without naming a preset makes `preset` null |
| `POST /scan[?query]` | walk and **cache** the matches (never deletes); responds like `GET /results` |
| `GET /results[?query]` | the cached scan viewed through filters, no walking |
| `POST /clean` | act on the cached matches that pass a filter (below) |
| `POST /restore` | replay the trash manifest (`settings.trash` or `{"trash":"DIR"}`); needs `--allow-exec` |
| `POST /shutdown` | reply, then exit |

### Tuning the filter, three layers

1. **Rule parameters** (`params`): change what the compiled rule selects
   (`stale_days`, `exts`, `names`, ...); takes effect on the next `POST /scan`.
2. **Scan settings** (`paths`, `hidden`, `max_depth`): where the walk goes.
3. **View filters** on the cache, free to change and repeat without
   re-walking: `min_size`, `max_size` (bytes), `ext` (comma list),
   `kind` (file|dir|symlink), `name` / `path` (substring), `older_than_days`,
   `newer_than_days`, `exclude` (exact path, repeatable). Plus
   `view=summary|detail`, `sort=path|size|modified|name`, `order=asc|desc`,
   `limit` (default 100, max 5000), `offset`.

`GET /results?ext=log,tmp&min_size=1048576&sort=size&limit=20` answers:

```
{"ok":true,"cached":true,"scanned_at":...,
 "total_matched":N,"total_bytes":B,          # the whole cache
 "matched":n,"bytes":b,                      # the selection: what a clean would remove
 "by_kind":{"file":{"matched":..,"bytes":..}},
 "by_ext":[{"ext":"log","matched":..,"bytes":..}],   # top 20 by bytes
 "by_root":[{"root":"...","matched":..,"bytes":..}],
 "offset":0,"limit":20,"returned":20,
 "matches":[{"path":"...","size":..,"kind":"file","modified":..,"age_days":..}]}
```

`view=summary` drops `matches` (the "total reclaimable space" view).

### Deciding to delete

`POST /clean` body: `{"filter":{...same keys...},"dry_run":bool,"expect_matched":N,"expect_bytes":B}`.

- `dry_run:true` returns `would_delete` (the summary above) and touches nothing;
  it is allowed even without `--allow-exec`.
- `expect_matched` / `expect_bytes` pin what the client reviewed: if the
  selection differs, the answer is `409` with the actual numbers and nothing is
  touched.
- A real clean needs `--allow-exec` (403 otherwise). It acts only on the
  selection (or everything cached if no filter) and reports
  `selected/deleted/missing/failed/bytes/remaining`. Unselected matches stay
  cached for another round; with `trash` set, matches move there instead
  (restore with `POST /restore`).

```
curl -sX POST $A/scan?view=summary                         # how much could go?
curl "$A/results?ext=log&older_than_days=30&sort=size"     # look closer
curl -sX PUT  $A/settings -d '{"params":{"stale_days":3}}' # loosen the rule itself
curl -sX POST $A/scan?view=summary
curl -sX POST $A/clean -d '{"filter":{"ext":"log"},"dry_run":true}'
curl -sX POST $A/clean -d '{"filter":{"ext":"log"},"expect_matched":12,"expect_bytes":3489192}'
curl -sX POST $A/shutdown
```

Presets file: `{"default":"name","presets":{"name":{"program":0,"paths":[...],
"hidden":false,"max_depth":null,"trash":null,"params":{...}}}}`; relative paths
resolve against the file's directory.

End-to-end smoke (fake files, launch, query, delete, exit; curl only):
`bash tests/e2e-server.sh` (needs `lmscript`).

Tests: `cargo test --test server` (protocol tests need nothing; the example
tests drive every `examples/*.lms` through `lmscript` and need `LMSCRIPT_BIN`
or `lmscript` on PATH, `LMCLEAN_REQUIRE_EXAMPLES=1` makes them mandatory).

## Layout

- `src/main.rs` — CLI, R42 flags, load/run/dispatch, exit protocol
- `src/host.rs` — `File` type, registry wiring, arena handles, `print`/`now`
- `src/walk.rs` — std::fs recursive walker (sorted, hidden-skip, depth limit)
- `src/trash.rs` — trash moves with manifest, restore
- `src/server.rs`, `src/view.rs`, `src/json.rs` — `--serve` REST mode, result filters/summaries, minimal JSON reader
- `src/frame.rs` — terminal return frame
- `examples/` — driver scripts `clean-tmp.lms`, `clean-build-dirs.lms`,
  `clean-ignore.lms`, `clean-stale-build-dirs.lms`, plus `serve-all.lms` and
  `serve-presets.json` (all four programs behind one server)
- `tests/server.rs` — REST integration tests (protocol + every example)
- `test.sh` — the gate: unit tests, release build, example compilation,
  end-to-end dry-run/exec/trash/restore/exit-code/hidden checks

Run the gate with `bash test.sh`.
