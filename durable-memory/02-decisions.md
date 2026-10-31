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
