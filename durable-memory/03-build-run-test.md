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
  `method File.rsize() -> int` in `--dsl-signatures`).

## Compiling rules

The mono compiler works and is the default in test.sh:

```
/home/ubuntu/lmctl-mono/lmscript/bin/lmscript examples/clean-tmp.lms target/clean-tmp.dslb
```

(`savedsl(program, ARGV[0])` in the .lms source takes the output path from
ARGV[0].) The released `lmscript` 0.2.444 on PATH was not needed; override with
`LMSCRIPT_BIN=... bash test.sh` if the mono build is unavailable.

`lmclean --dsl-schema | cmp - clean.dsl.json` must hold: the embedded schema
is byte-identical to the file programs are compiled against.
