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

## Compiling and running rules

There is no standalone `.dslb` export any more (`savedsl` was removed from the
compiler). Each `examples/*.lms` is a *driver*: after the `_lmcode_` block it
calls `run_lmcode(program, {command: [ARGV[0], "--lmcode", "$DSLFILE", ...]})`,
so `lmscript examples/NAME.lms HOST [HOST ARGS]` compiles and
runs in one step. Install the compiler with `npm install @lmctl-ai/lmctl`
(0.2.450 is what CI and these notes were verified with; set `LMSCRIPT_BIN`).

Frame protocol the consumer enforces: the host's last stdout output is
`LMCODE_RETURN_BEGIN\n<compact JSON>\nLMCODE_RETURN_END\n`; earlier stdout is
passthrough. Success is `{"ok":true,"result":V}` (builder: `..."result":[..],"parts":N`
in that order); failure is compact `{"ok":false,"phase":..,"code":..,"message":..}`.
dslvm's own `report_ok`/`error_json` emit spaces after colons and no sentinels,
which the consumer rejects, so `src/frame.rs` writes frames itself. Builders
(`lmcode_build([host...])` + `b.add(p, ["--lmcode","$DSLFILE"])`) invoke the host
once with repeated `--lmcode FILE` pairs. `run_lmcode` relays host stdout only
when the host exits, hence `--serve --port-file`.

`lmclean --lmcode-schema | cmp - clean.dsl.json` must hold: the embedded schema
is byte-identical to the file programs are compiled against.

## CI (`.github/workflows/release.yml`)

Five-platform matrix (ubuntu-24.04, ubuntu-24.04-arm, macos-15,
macos-15-intel, windows-2022), then a publish job (main only) creating release
`v0.1.<run_number>` with the five binaries, LICENSE and SHA256SUMS. Each build
job installs `@lmctl-ai/lmctl@$LMCTL_VERSION` (pinned in the workflow env),
runs `cargo test --test server` (REST integration tests over every example,
`LMCLEAN_REQUIRE_EXAMPLES=1`), then the full `bash test.sh` gate. Only the
Linux x64 run has been exercised by hand; macOS/Windows/arm are unverified.

- **dslvm pin**: `Cargo.toml` pins the runtime by git rev
  (`f6b50c41905e925c8f662ec62e224226de8c1591`); lmcode-rust main later removed
  `report_app_failure`, so a rev bump must come with the host API fix.
- **Version stamping**: `build.rs` bakes `LMCLEAN_VERSION` into `--version`.
- **Portable mtimes**: `touch_age DAYS PATH` in test.sh uses GNU `touch -d`
  first, BSD `date -v` + `touch -t` on macOS.
- Unit test `walk::tests::rsize_skips_unreadable_subtrees` needs a non-root
  user (root ignores `chmod 000`); skip it when running as root.
