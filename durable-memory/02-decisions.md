# Decisions

- **Schema surface** (`clean.dsl.json`, name `lmclean_v1`): host type `File`
  (path/name/extension as byte strings; size/depth/modified as int;
  is_file/is_dir/is_symlink as bool), interfaces `Filter(File) -> bool`,
  `Report(File) -> void` and `Ignore(File) -> bool`, functions
  `print(string) -> void` (stderr, keeping stdout machine-clean) and
  `now() -> int` (epoch seconds). No `context` section: the mono compiler
  accepts its absence and the host declares no context variables.
- **Registry**: `Registry::new()` (the built-in std catalog) plus our own
  `print` (stderr) and `now`. Std is enabled so rules may import any of the
  implicit 72-signature lmcode std catalog the compiler seeds; the schema
  itself stays minimal.
- **Dispatch**: entries annotated `select` must implement Filter, `report`
  must implement Report, `ignore` must implement Ignore; a violation is
  APP_FAIL (exit 4). Multiple entries of one annotation are OR-ed. Each
  candidate's `File` handle is inserted into the context and taken back after
  the visit, so the arena does not grow with the tree size.
- **No .gitignore, ever**: lmclean ships no ignore-file support and no
  built-in ignore rules. Pruning is the lmcode program's job: `@ignore`
  entries run for every candidate *before* any `@select` entry; a true result
  skips the candidate entirely (not filtered, not listed) and bars descent
  into a directory. Zero `@ignore` entries means a fully unchecked search,
  subject only to the explicit `--hidden` / `--max-depth` CLI flags.
  Rationale: ignore policy is rule logic, belongs in versioned .lms sources
  compiled against the schema, and must never silently change between runs of
  the same program.
- **Walk**: children of a search root have depth 0; entries are sorted by raw
  name bytes; hidden (dot) names are skipped unless `--hidden`; symlinks use
  `symlink_metadata` and are never descended; unreadable dirs are a stderr note
  and a skip, never an abort. A matched directory is not descended into at
  all (its descendants are never re-filtered).
- **Deletion**: matched files/symlinks are unlinked; matched directories are
  removed recursively. Deletion failures are stderr notes and flip the final
  line to an APP_FAIL (exit 4) instead of the summary.
- **Trash**: destination preserves the path relative to its search root;
  collisions get a `.N` suffix recorded in the manifest line
  `{"stored":"...","original":"..."}` (byte-exact escaping). Moves use
  `rename` with a copy+remove fallback on EXDEV. Restore replays the manifest
  in reverse order, recreating parent dirs.
- **Restore mode** keeps one code path: `--lmcode FILE` is still required and the
  program is loaded (so a bad file still exits 3), but the body is not run and
  nothing is walked.
- **Output protocol**: stdout carries only JSON lines (byte-exact escaping like
  the sample host); `print()` and all diagnostics go to stderr.
- **CI releases** (`.github/workflows/release.yml`, mirroring
  lmcode-rust-sample): five-platform build matrix, `LMCLEAN_VERSION` =
  `0.1.<run_number>` baked in by `build.rs`, publish job on main creates
  `v<VERSION>` with binaries + LICENSE + SHA256SUMS. No THIRD_PARTY_NOTICES.md:
  lmclean has no third-party code — its only dependency is the first-party
  dslvm, pinned by git rev in Cargo.toml (see 03), not tracking moving main.
- **Examples are lmscript drivers**: `savedsl` is gone, so every example ends
  with a `run_lmcode` call that launches the host with `--lmcode $DSLFILE`
  and forwards ARGV. The host therefore speaks the return-frame protocol
  (`src/frame.rs`, see 03) instead of printing a bare summary line; the old
  `{"ok":true,"matched":..}` line became the frame's `result`.
- **Rule parameters**: `param_int`/`param_str` host functions (schema change)
  read a shared map filled from `--param` or the server's `params` setting, so
  clients can tune the compiled rule's thresholds/name lists without
  recompiling. The schema hash changed with them; programs are recompiled on
  every run, so nothing on disk is stale.
- **Self-contained recursive size**: `rsize` is computed natively with std::fs
  (`walk::rsize`), never by shelling out to `du`/`df` — the host must stay
  zero-dependency and portable, and sizes must be gate-deterministic. Exact
  semantics: the entry itself plus, for directories, all descendants
  recursively, counting only non-directory entries (symlinks contribute their
  own `symlink_metadata` len, never followed). Directory inode sizes are
  excluded, so results are filesystem-independent (unlike `du -b`). Hard links
  are counted per link (no inode dedup). Unreadable subtrees are skipped. The
  per-match `"size"` and summary `"bytes"` report rsize, computed once at
  match time only — never for non-matches, and before any deletion.
- **Server mode (`--serve`)**: std-only single-threaded HTTP/1.1 (no new
  dependencies), hand-written JSON reader (`src/json.rs`). State = settings
  (program ref, paths, hidden, max_depth, trash, params) + verified `Program`
  + cached scan. Programs arrive as numbered `--lmcode` slots (one per
  `run_lmcode` program/builder part); presets name a slot or a file path.
  `POST /scan` caches matches; `GET /results` slices the cache with
  filters/sort/paging/summaries (`src/view.rs`) without re-walking; `POST
  /clean` acts on a filtered selection, supports `dry_run` and
  `expect_matched/expect_bytes` pinning (409 on drift), and keeps unselected
  matches cached. Any settings change drops the cache. Real deletes are opt-in
  at start (`--allow-exec`); `dry_run` is always allowed; non-loopback bind
  requires `--token`. Lifetime: `POST /shutdown` or `--idle-timeout` (default
  300s). `--port-file` because run_lmcode buffers host stdout until exit. The
  scan loop in `server.rs` duplicates the CLI's ignore/select/report dispatch
  rather than refactoring `main.rs::Sweep`; keep the two in step.
