# Purpose and place

lmclean is a scriptable file search & cleanup tool in the lmcode ecosystem,
shaped after the reference host `/home/ubuntu/lmcode-rust-sample`: a native
executable that loads verified lmcode bytecode v6 (`.dslb`, compiled externally
by lmscript) and runs it. The user's lmcode program supplies `@select` rule
entries implementing the SAM `Filter(File) -> bool` interface; lmclean walks
the filesystem fd-style, calls the filter once per candidate, and the boolean
decides deletion. `@report` entries (`Report(File) -> void`) run once per
matched file for auditing.

Safety model is rip-style: dry-run is the default, `--exec` deletes,
`--exec --trash DIR` moves matches into DIR with a `manifest.jsonl`, and
`--trash DIR --restore` puts them back.

Hard constraints inherited from the ecosystem: Rust edition 2021, zero
crates.io dependencies (only `dslvm` by path from `../lmcode-rust/dslvm`),
bytecode v6 only, and the schema bytes of `clean.dsl.json` are SHA-256-bound
into every compiled program — treat that file as immutable once programs are
compiled against it.

The pre-existing `clean.lmc` file at the repo root is user data, not ours;
never modify or delete it.
