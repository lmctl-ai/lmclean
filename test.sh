#!/usr/bin/env bash
# lmclean gate: unit tests, release build, example compilation, end-to-end
# dry-run / exec / trash / restore, exit-code protocol, hidden handling,
# JSON stdout validity.
set -euo pipefail
cd "$(dirname "$0")"

# shellcheck source=/dev/null
[ -f ~/.cargo/env ] && source ~/.cargo/env

BIN=target/release/lmclean
LMSCRIPT_BIN="${LMSCRIPT_BIN:-/home/ubuntu/lmctl-mono/lmscript/bin/lmscript}"
# Set LMCLEAN_SKIP_EXAMPLES=1 to bypass every lmscript-dependent section
# (example compilation and the dslb-driven end-to-end runs). Used in CI,
# where the released lmscript cannot export .dslb bytecode (savedsl was
# removed from the public compiler); cargo tests, the release build and the
# --version/--lmcode-schema/--lmcode-signatures checks still run.
LMCLEAN_SKIP_EXAMPLES="${LMCLEAN_SKIP_EXAMPLES:-}"

# python3 is not on PATH as "python3" everywhere (Windows Git Bash).
PYTHON=python3
command -v python3 >/dev/null 2>&1 || PYTHON=python

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "== $*"; }

# touch_age DAYS PATH — set mtime DAYS days in the past. GNU touch first
# (Linux, Git Bash on Windows), BSD date -v fallback (macOS).
touch_age() {
  if touch -d "$1 days ago" "$2" 2>/dev/null; then return 0; fi
  touch -t "$(date -v-"$1"d +%Y%m%d%H%M.%S)" "$2"
}

note "cargo test --locked"
cargo test --locked --quiet

note "cargo build --release --locked"
cargo build --release --locked --quiet
[ -x "$BIN" ] || fail "release binary missing"
"$PYTHON" tests/check-host-flags.py clean.dsl.json "$BIN" || fail "hard host flag cutover"

if [ -z "$LMCLEAN_SKIP_EXAMPLES" ]; then

note "compile examples with $LMSCRIPT_BIN"
"$LMSCRIPT_BIN" examples/clean-tmp.lms target/clean-tmp.dslb
"$LMSCRIPT_BIN" examples/clean-build-dirs.lms target/clean-build-dirs.dslb
"$LMSCRIPT_BIN" examples/clean-ignore.lms target/clean-ignore.dslb
"$LMSCRIPT_BIN" examples/clean-stale-build-dirs.lms target/clean-stale-build-dirs.dslb
[ -s target/clean-tmp.dslb ] || fail "clean-tmp.dslb missing"
[ -s target/clean-build-dirs.dslb ] || fail "clean-build-dirs.dslb missing"
[ -s target/clean-ignore.dslb ] || fail "clean-ignore.dslb missing"
[ -s target/clean-stale-build-dirs.dslb ] || fail "clean-stale-build-dirs.dslb missing"

assert_json_lines() {
  # Every stdout line must parse as JSON.
  "$PYTHON" -c '
import json, sys
for line in sys.stdin:
    line = line.strip()
    if line:
        json.loads(line)
' || fail "stdout line is not valid JSON"
}

build_tree() {
  local tree="$1"
  mkdir -p "$tree/sub" "$tree/.hid" "$tree/app/node_modules/pkg" "$tree/app/src"
  echo keep    > "$tree/keep.txt"
  echo old     > "$tree/old.tmp";   touch_age 10 "$tree/old.tmp"
  echo stale   > "$tree/stale.log"; touch_age 30 "$tree/stale.log"
  echo new     > "$tree/new.tmp"
  echo nested  > "$tree/sub/nested.tmp"; touch_age 10 "$tree/sub/nested.tmp"
  echo hidden  > "$tree/.hid/h.tmp";     touch_age 10 "$tree/.hid/h.tmp"
  echo dotfile > "$tree/.dotfile.tmp";   touch_age 10 "$tree/.dotfile.tmp"
  echo module  > "$tree/app/node_modules/pkg/index.js"
  echo code    > "$tree/app/src/main.rs"
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- 1. dry-run (default): lists the right matches, removes nothing ---------
note "dry-run"
TREE="$WORK/tree1"; build_tree "$TREE"
OUT="$($BIN --lmcode target/clean-tmp.dslb "$TREE")" || fail "dry-run exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
[ "$(printf '%s\n' "$OUT" | grep -c '"plan":"delete"')" = 3 ] \
  || fail "dry-run should plan 3 matches (old.tmp, stale.log, sub/nested.tmp)"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/old.tmp"'    || fail "old.tmp not planned"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/stale.log"'  || fail "stale.log not planned"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/sub/nested.tmp"' || fail "nested.tmp not planned"
printf '%s\n' "$OUT" | grep -q 'new.tmp'    && fail "new.tmp must not match (too fresh)"
printf '%s\n' "$OUT" | grep -q 'h.tmp'      && fail "hidden file must be skipped by default"
printf '%s\n' "$OUT" | grep -q '"dry_run":true' || fail "summary must say dry_run true"
# Exact sizes: old.tmp=4 ("old\n"), stale.log=6 ("stale\n"), nested.tmp=7 ("nested\n").
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/old.tmp","size":4' || fail "old.tmp size must be 4"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/stale.log","size":6' || fail "stale.log size must be 6"
printf '%s\n' "$OUT" | grep -q '"bytes":17' || fail "summary bytes must be the exact sum 17"
for f in old.tmp stale.log new.tmp sub/nested.tmp .hid/h.tmp; do
  [ -f "$TREE/$f" ] || fail "dry-run removed $f"
done

# --- 2. --exec removes exactly the matches ----------------------------------
note "--exec"
TREE="$WORK/tree2"; build_tree "$TREE"
OUT="$($BIN --lmcode target/clean-tmp.dslb --exec "$TREE")" || fail "exec exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
[ "$(printf '%s\n' "$OUT" | grep -c '"deleted":"unlink"')" = 3 ] || fail "exec should unlink 3"
printf '%s\n' "$OUT" | grep -q '"dry_run":false' || fail "summary must say dry_run false"
for f in old.tmp stale.log sub/nested.tmp; do
  [ ! -e "$TREE/$f" ] || fail "exec did not remove $f"
done
for f in keep.txt new.tmp .hid/h.tmp app/src/main.rs; do
  [ -f "$TREE/$f" ] || fail "exec removed non-match $f"
done

# --- 3. --trash moves, manifest records, --restore returns byte-exact -------
note "--trash + --restore"
TREE="$WORK/tree3"; build_tree "$TREE"
TRASH="$WORK/trash3"
mkdir -p "$WORK/backup3/sub"
cp "$TREE/old.tmp" "$WORK/backup3/old.tmp"
cp "$TREE/stale.log" "$WORK/backup3/stale.log"
cp "$TREE/sub/nested.tmp" "$WORK/backup3/sub/nested.tmp"
OUT="$($BIN --lmcode target/clean-tmp.dslb --exec --trash "$TRASH" "$TREE")" || fail "trash exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
[ "$(printf '%s\n' "$OUT" | grep -c '"deleted":"trash"')" = 3 ] || fail "trash should move 3"
[ ! -e "$TREE/old.tmp" ] || fail "old.tmp not moved out"
[ -f "$TRASH/old.tmp" ] || fail "old.tmp not in trash"
[ -f "$TRASH/sub/nested.tmp" ] || fail "nested.tmp not in trash preserving rel path"
[ -f "$TRASH/manifest.jsonl" ] || fail "manifest missing"
[ "$(wc -l < "$TRASH/manifest.jsonl")" = 3 ] || fail "manifest should have 3 lines"
assert_json_lines < "$TRASH/manifest.jsonl"
OUT="$($BIN --lmcode target/clean-tmp.dslb --trash "$TRASH" --restore)" || fail "restore exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
[ "$(printf '%s\n' "$OUT" | grep -c '^{"restored":"')" = 3 ] || fail "restore should report 3"
printf '%s\n' "$OUT" | grep -q '"ok":true,"restored":3' || fail "restore summary missing"
cmp "$TREE/old.tmp" "$WORK/backup3/old.tmp"       || fail "old.tmp not byte-exact after restore"
cmp "$TREE/stale.log" "$WORK/backup3/stale.log"   || fail "stale.log not byte-exact after restore"
cmp "$TREE/sub/nested.tmp" "$WORK/backup3/sub/nested.tmp" || fail "nested.tmp not byte-exact after restore"
[ ! -e "$TRASH/old.tmp" ] || fail "restore left old.tmp in trash"

# --- 4. exit-code protocol ---------------------------------------------------
note "exit codes"
"$BIN" --bogus >/dev/null 2>&1 && fail "unknown flag should fail" || code=$?
[ "${code:-0}" = 2 ] || fail "unknown flag exit ${code:-0}, want 2"
"$BIN" --lmcode target/clean-tmp.dslb --trash /tmp/x >/dev/null 2>&1 && fail "--trash without --exec should fail" || code=$?
[ "${code:-0}" = 2 ] || fail "--trash without --exec exit ${code:-0}, want 2"
"$BIN" --lmcode target/clean-tmp.dslb --trash /tmp/x --restore --exec >/dev/null 2>&1 && fail "--restore with --exec should fail" || code=$?
[ "${code:-0}" = 2 ] || fail "--restore with --exec exit ${code:-0}, want 2"
echo "this is not bytecode" > "$WORK/garbage.dslb"
"$BIN" --lmcode "$WORK/garbage.dslb" >/dev/null 2>&1 && fail "garbage bytecode should fail" || code=$?
[ "${code:-0}" = 3 ] || fail "garbage bytecode exit ${code:-0}, want 3"
OUT=$("$BIN" --lmcode "$WORK/garbage.dslb" 2>/dev/null) || true
printf '%s\n' "$OUT" | assert_json_lines
printf '%s\n' "$OUT" | grep -q '"phase": *"load"' || fail "load error must be phase load"

# --- 5. hidden files ----------------------------------------------------------
note "hidden handling"
TREE="$WORK/tree5"; build_tree "$TREE"
OUT="$($BIN --lmcode target/clean-tmp.dslb "$TREE")"
printf '%s\n' "$OUT" | grep -q 'h.tmp' && fail "default walk included hidden file"
OUT="$($BIN --lmcode target/clean-tmp.dslb --hidden "$TREE")"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/.hid/h.tmp"' || fail "--hidden should include .hid/h.tmp"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/.dotfile.tmp"' || fail "--hidden should include .dotfile.tmp"

# --- 6. directory rules (clean-build-dirs) ------------------------------------
note "directory matching"
TREE="$WORK/tree6"; build_tree "$TREE"
OUT="$($BIN --lmcode target/clean-build-dirs.dslb "$TREE")" || fail "dir dry-run exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
[ "$(printf '%s\n' "$OUT" | grep -c '"plan":"delete"')" = 1 ] || fail "should match exactly node_modules"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/app/node_modules"' || fail "node_modules not matched"
# index.js is "module\n" = 7 bytes; a matched dir's size is its recursive content sum.
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/app/node_modules","size":7' \
  || fail "node_modules size must be the recursive sum 7"
printf '%s\n' "$OUT" | grep -q '"bytes":7' || fail "dir summary bytes must be 7"
printf '%s\n' "$OUT" | grep -q 'index.js' && fail "descendants of a matched dir must not be re-filtered"
OUT="$($BIN --lmcode target/clean-build-dirs.dslb --exec "$TREE")" || fail "dir exec exit != 0"
[ ! -e "$TREE/app/node_modules" ] || fail "node_modules not removed"
[ -f "$TREE/app/src/main.rs" ] || fail "unrelated dir removed"

# --- 7. --max-depth ------------------------------------------------------------
note "--max-depth"
TREE="$WORK/tree7"; build_tree "$TREE"
OUT="$($BIN --lmcode target/clean-tmp.dslb --max-depth 0 "$TREE")"
printf '%s\n' "$OUT" | grep -q 'sub/nested.tmp' && fail "--max-depth 0 should not reach sub/"
[ "$(printf '%s\n' "$OUT" | grep -c '"plan":"delete"')" = 2 ] || fail "--max-depth 0 should match 2"

# --- 8. @ignore rules prune the tree --------------------------------------------
note "@ignore pruning"
TREE="$WORK/tree8"
mkdir -p "$TREE/.git/hooks"
echo vcs > "$TREE/.git/hooks/x.tmp"; touch_age 10 "$TREE/.git/hooks/x.tmp"
echo top > "$TREE/old.tmp";         touch_age 10 "$TREE/old.tmp"
# With the ignore program (.git pruned by rule, hidden dirs included via --hidden):
OUT="$($BIN --lmcode target/clean-ignore.dslb --hidden "$TREE")" || fail "ignore dry-run exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
printf '%s\n' "$OUT" | grep -q 'x.tmp' && fail "@ignore must prune .git/hooks/x.tmp from dry-run"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/old.tmp"' || fail "old.tmp should still match"
"$BIN" --lmcode target/clean-ignore.dslb --hidden --exec "$TREE" >/dev/null \
  || fail "ignore exec exit != 0"
[ -f "$TREE/.git/hooks/x.tmp" ] || fail "@ignore pruning: x.tmp must survive --exec"
[ ! -e "$TREE/old.tmp" ] || fail "old.tmp should have been deleted"
# A program with no @ignore entries sees everything (no built-in ignore rules):
OUT="$($BIN --lmcode target/clean-tmp.dslb --hidden "$TREE")" || fail "no-ignore dry-run exit != 0"
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/.git/hooks/x.tmp"' \
  || fail "program without @ignore must see .git contents with --hidden"
# An @ignore entry with the wrong interface is an application failure:
cat > target/bad-ignore.lms <<'EOF'
program = _lmcode_ schema="../clean.dsl.json";
@ignore
fn wrong(f: File) -> void implements Report {
  print("not an ignore");
}
end _lmcode_
savedsl(program, ARGV[0]);
EOF
"$LMSCRIPT_BIN" target/bad-ignore.lms target/bad-ignore.dslb || fail "bad-ignore compile failed"
"$BIN" --lmcode target/bad-ignore.dslb "$TREE" >/dev/null 2>&1 && fail "wrong @ignore interface should fail" || code=$?
[ "${code:-0}" = 4 ] || fail "wrong @ignore interface exit ${code:-0}, want 4"

# --- 9. stderr is separate; print() does not pollute stdout --------------------
note "stdout purity"
TREE="$WORK/tree8"; build_tree "$TREE"
"$BIN" --lmcode target/clean-tmp.dslb "$TREE" >"$WORK/out8" 2>"$WORK/err8"
assert_json_lines < "$WORK/out8"
grep -q 'lmclean matched' "$WORK/err8" || fail "@report print() should write to stderr"
grep -q 'lmclean matched' "$WORK/out8" && fail "print() leaked into stdout"

# --- 10. stale build dirs: 24h age rule ----------------------------------------
note "stale build dirs (24h)"
TREE="$WORK/tree10"
mkdir -p "$TREE/oldproj/target" "$TREE/fresh/dist" "$TREE/src/build"
touch_age 2 "$TREE/oldproj/target"
OUT="$($BIN --lmcode target/clean-stale-build-dirs.dslb "$TREE")" || fail "stale-dirs dry-run exit != 0"
printf '%s\n' "$OUT" | assert_json_lines
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/oldproj/target"' || fail "2-day-old target not matched"
# An empty matched dir has rsize 0 (directory inode sizes are never counted).
printf '%s\n' "$OUT" | grep -q '"path":"'"$TREE"'/oldproj/target","size":0' || fail "empty target dir size must be 0"
printf '%s\n' "$OUT" | grep -q 'fresh/dist' && fail "fresh dist must not match the 24h rule"
printf '%s\n' "$OUT" | grep -q 'src/build' && fail "fresh build must not match the 24h rule"

else
note "LMCLEAN_SKIP_EXAMPLES=1: skipping example compilation and dslb end-to-end sections"
fi

# --- 11. schema and signatures flags ---------------------------------------------
note "R42 flags"
[ "$("$BIN" --version)" = "lmclean ${LMCLEAN_VERSION:-0.1.0}" ] || fail "--version output"
"$BIN" --lmcode-schema | cmp - clean.dsl.json || fail "--lmcode-schema must print exact schema bytes"
"$BIN" --lmcode-schema --json | cmp - clean.dsl.json || fail "--lmcode-schema --json must print exact schema bytes"
# Capture before grepping: piping the binary into grep -q can close stdout
# early (SIGPIPE) and trip pipefail.
SIGS="$("$BIN" --lmcode-signatures)" || fail "--lmcode-signatures exit != 0"
printf '%s\n' "$SIGS" | grep -q '^method File.modified() -> int$' || fail "--lmcode-signatures missing File.modified"
printf '%s\n' "$SIGS" | grep -q '^method File.rsize() -> int$' || fail "--lmcode-signatures missing File.rsize"
printf '%s\n' "$SIGS" | grep -q '^fn now() -> int$' || fail "--lmcode-signatures missing now"

echo
echo "PASS: all lmclean gate checks passed"
