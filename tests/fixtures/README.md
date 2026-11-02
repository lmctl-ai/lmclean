# Prebuilt example bytecode

`tests/server.rs` drives each rule pack in `examples/` through `lmclean --serve`
using compiled bytecode from this directory:

    clean-tmp.dslb  clean-build-dirs.dslb  clean-ignore.dslb  clean-stale-build-dirs.dslb

CI cannot compile the `.lms` sources (the released lmscript removed `savedsl`),
so these files are committed. Bytecode is bound to the SHA-256 of
`clean.dsl.json`: regenerate all four whenever the schema changes, e.g. on a
machine with a savedsl-capable compiler:

    for n in clean-tmp clean-build-dirs clean-ignore clean-stale-build-dirs; do
      lmscript examples/$n.lms tests/fixtures/$n.dslb
    done

Without them the example tests skip locally, and fail in CI
(`LMCLEAN_REQUIRE_EXAMPLES=1`).
