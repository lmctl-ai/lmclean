# Build, run, test

cargo is not on PATH: `source ~/.cargo/env` first (or call
`/home/ubuntu/.cargo/bin/cargo`).

- Build: `cargo build --release --locked`
- Lint: `cargo clippy --all-targets` (clean)
- Unit tests: `cargo test --locked` (walker sorting/hidden/depth/symlink,
  `rsize` exact sums / symlink own-len / unreadable-subtree skip, and trash
  manifest round-trip; scratch dirs under `target/`)
- Gate: `bash test.sh` — unit tests (walker sorting/hidden/depth/symlink,
  trash manifest round-trip, `rsize` exact sums / symlink own-len / unreadable
  skip; scratch dirs under `target/`), release build, compilation of all four
  examples (`clean-tmp`, `clean-build-dirs`, `clean-ignore`,
  `clean-stale-build-dirs`), end-to-end
  dry-run/exec/trash/restore on a scratch tree, exit-code protocol (2 bad
  flags / 3 garbage bytecode / 4 wrong `@ignore` interface / 0 ok), hidden
  handling, max-depth, `@ignore` pruning of `.git` (and proof that a program
  without `@ignore` sees everything under `--hidden`), exact recursive sizes
  in per-match `"size"` and summary `"bytes"` (4+6+7=17 for the tmp fixture,
  7 for the node_modules fixture, 0 for an empty matched dir), stale-build-dir
  24h age rule, stdout-is-JSON purity, R42 flags (including
  `method File.rsize() -> int` in `--lmcode-signatures`).

## Compiling rules

The mono compiler works and is the default in test.sh:

```
/home/ubuntu/lmctl-mono/lmscript/bin/lmscript examples/clean-tmp.lms target/clean-tmp.dslb
```

(`savedsl(program, ARGV[0])` in the .lms source takes the output path from
ARGV[0].) The released `lmscript` 0.2.444 on PATH was not needed; override with
`LMSCRIPT_BIN=... bash test.sh` if the mono build is unavailable.

**BROKEN since the mono rebuild of 2026-10-31 (commit 4cb7bdb3, "Make lmcode
execution explicit and keep private files owned until drainage")**: `savedsl`
was removed from both the mono compiler and the released lmscript (verified
0.2.451; bytecode export is now private to `run_lmcode`). `bash test.sh` fails
at the example-compilation step until the examples are rewritten or an export
path returns. Prebuilt `target/*.dslb` from 2026-10-30 remain valid; running
`LMSCRIPT_BIN=/bin/true bash test.sh` reuses them and the full gate passes.

`lmclean --lmcode-schema | cmp - clean.dsl.json` must hold: the embedded schema
is byte-identical to the file programs are compiled against.

## CI (`.github/workflows/release.yml`)

Five-platform matrix (ubuntu-24.04, ubuntu-24.04-arm, macos-15,
macos-15-intel, windows-2022) mirroring lmcode-rust-sample, then a publish job
(main only) creating release `v0.1.<run_number>` with the five binaries,
LICENSE and SHA256SUMS. No THIRD_PARTY_NOTICES.md: lmclean has no third-party
code (only dslvm, which is first-party lmctl-ai).

- **dslvm pin**: `Cargo.toml` pins the runtime by git rev
  (`dslvm = { git = "https://github.com/lmctl-ai/lmcode-rust", rev =
  "f6b50c41905e925c8f662ec62e224226de8c1591" }`), so CI needs no side-by-side
  checkout — `cargo build --locked` fetches the exact pinned dslvm. Note that
  lmcode-rust main HEAD (`94d62fa1bddcba71d787bab15ff4954822b208e3` as of
  2026-11-01) removed `report_app_failure` and does not compile lmclean; a
  rev bump must come with the host API fix.
- **LMCLEAN_SKIP_EXAMPLES=1** is set in CI: with `savedsl` gone from the
  released compiler, the dslb-driven end-to-end sections cannot run there. CI
  keeps cargo tests, the release build, `tests/check-host-flags.py` and the
  `--version/--lmcode-schema/--lmcode-signatures` checks on all five platforms.
  No Node/lmctl install is needed in CI for the same reason.
- **Version stamping**: `build.rs` bakes `LMCLEAN_VERSION` (set to
  `0.1.${{ github.run_number }}` in the workflow) into `--version`, falling
  back to `CARGO_PKG_VERSION` locally; test.sh asserts
  `lmclean ${LMCLEAN_VERSION:-0.1.0}`.
- **Portable mtimes**: `touch_age DAYS PATH` in test.sh uses GNU `touch -d`
  first (Linux, Git Bash on Windows) and falls back to BSD `date -v` +
  `touch -t` (macOS), so the full gate can run on macOS too.
