#!/usr/bin/env bash
# End-to-end smoke test of `lmclean --serve`, black-box over HTTP with curl:
#   create fake files -> launch the server -> query it several ways ->
#   preview and perform deletes -> shut down and check the exit.
# Then a second launch proves the idle timeout exits on its own.
# Needs: lmscript ($LMSCRIPT_BIN, default `lmscript`), curl, python, and the
# release binary (built here if missing). Runs on Linux, macOS and Git Bash.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=/dev/null
[ -f ~/.cargo/env ] && source ~/.cargo/env

BIN=target/release/lmclean
LMSCRIPT_BIN="${LMSCRIPT_BIN:-lmscript}"
PYTHON=python3
command -v python3 >/dev/null 2>&1 || PYTHON=python

fail() { echo "FAIL: $*" >&2; [ -f "${WORK:-/nonexistent}/server.err" ] && tail -n 20 "$WORK/server.err" >&2; exit 1; }
note() { echo "== $*"; }

[ -x "$BIN" ] || [ -x "$BIN.exe" ] || cargo build --release --locked --quiet
HOST_BIN="$(pwd -W 2>/dev/null || pwd)/$BIN"
[ -f "$BIN.exe" ] && HOST_BIN="$HOST_BIN.exe"

WORK="$(mktemp -d)"
# Git Bash: use the native form so paths match what the host reports.
if command -v cygpath >/dev/null 2>&1; then WORK="$(cygpath -m "$WORK")"; fi
SERVER_PID=""
ADDR=""
cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    [ -n "$ADDR" ] && curl -s --noproxy '*' --max-time 5 -X POST "http://$ADDR/shutdown" >/dev/null 2>&1 || true
    sleep 1
    kill "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

touch_age() {
  if touch -d "$1 days ago" "$2" 2>/dev/null; then return 0; fi
  touch -t "$(date -v-"$1"d +%Y%m%d%H%M.%S)" "$2"
}
# mkfile PATH BYTES [AGE_DAYS]
mkfile() {
  mkdir -p "$(dirname "$1")"
  head -c "$2" /dev/zero > "$1"
  if [ -n "${3:-}" ]; then touch_age "$3" "$1"; fi
}

# call METHOD PATH [BODY] -> sets CODE and BODY
call() {
  local out
  out="$(curl -sS --noproxy '*' --max-time 30 -o "$WORK/body" -w '%{http_code}' \
    -X "$1" "http://$ADDR$2" ${3:+-d "$3"})" || fail "curl $1 $2 failed"
  CODE="$out"
  BODY="$(tr -d '\r' < "$WORK/body")"
}
# expect CODE_WANTED: assert the last status
expect() { [ "$CODE" = "$1" ] || fail "expected HTTP $1, got $CODE: $BODY"; }
# field PY_EXPR: evaluate an expression over the last JSON body as `d`
field() {
  printf '%s' "$BODY" | "$PYTHON" -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1" \
    || fail "cannot evaluate $1 on: $BODY"
}
assert_field() { # EXPR WANTED
  local got; got="$(field "$1" | tr -d '\r')"
  [ "$got" = "$2" ] || fail "$1: expected $2, got $got (body: $BODY)"
}

# --- 1. fake files --------------------------------------------------------------
note "create fake files"
TREE="$WORK/tree"
mkfile "$TREE/junk/a.tmp"        1000 10
mkfile "$TREE/junk/b.tmp"        3000 10
mkfile "$TREE/junk/c.log"        5000 30
mkfile "$TREE/junk/sub/d.log"    2000 40
mkfile "$TREE/junk/fresh.tmp"     100
mkfile "$TREE/keep/readme.txt"     50 100
mkfile "$TREE/app/node_modules/x/index.js" 700

cat > "$WORK/presets.json" <<EOF
{
  "default": "junk",
  "presets": {
    "junk":       { "program": 0, "paths": ["$TREE/junk"] },
    "build-dirs": { "program": 1, "paths": ["$TREE/app"] },
    "vcs-safe":   { "program": 2, "paths": ["$TREE/junk"], "hidden": true },
    "stale-dirs": { "program": 3, "paths": ["$TREE/app"] }
  }
}
EOF

# --- 2. launch --------------------------------------------------------------------
note "launch lmclean (all four example programs behind one server)"
"$LMSCRIPT_BIN" examples/serve-all.lms "$HOST_BIN" --serve \
  --presets "$WORK/presets.json" --allow-exec --idle-timeout 120 --port-file "$WORK/port" \
  > "$WORK/server.out" 2> "$WORK/server.err" &
SERVER_PID=$!
for _ in $(seq 300); do
  [ -s "$WORK/port" ] && break
  kill -0 "$SERVER_PID" 2>/dev/null || fail "server exited before listening"
  sleep 0.2
done
[ -s "$WORK/port" ] || fail "server never wrote its port file"
ADDR="$(tr -d '\r\n' < "$WORK/port")"
echo "listening on $ADDR"

# --- 3. query a few times -----------------------------------------------------------
note "query: health, presets, summary scan"
call GET /health;  expect 200; assert_field "d['ok']" True; assert_field "d['allow_exec']" True
call GET /presets; expect 200; assert_field "len(d['presets'])" 4; assert_field "d['active']" "junk"
call POST '/scan?view=summary'; expect 200
assert_field "d['matched']" 4          # a.tmp b.tmp c.log d.log (fresh.tmp is too new)
assert_field "d['bytes']" 11000
assert_field "'matches' in d" False
assert_field "[e['ext'] for e in d['by_ext']]" "['log', 'tmp']"   # ranked by bytes: log 7000 > tmp 4000

note "query: filtered, sorted and paged views of the cached scan"
call GET '/results?ext=log&sort=size'; expect 200
assert_field "d['cached']" True
assert_field "(d['matched'], d['bytes'], d['total_matched'])" "(2, 7000, 4)"
assert_field "d['matches'][0]['path'].endswith('c.log')" True
call GET '/results?min_size=2500&sort=size&order=asc&limit=1'; expect 200
assert_field "(d['matched'], d['returned'])" "(2, 1)"
assert_field "d['matches'][0]['path'].endswith('b.tmp')" True
call GET '/results?older_than_days=20&view=summary'; expect 200
assert_field "d['matched']" 2
call GET '/results?bogus=1'; expect 400

note "query: loosen the rule itself via params, then restore it"
call PUT /settings '{"params":{"stale_days":20}}'; expect 200
call GET /results; expect 404                                    # settings change drops the cache
call POST '/scan?view=summary'; expect 200
assert_field "(d['matched'], d['bytes'])" "(2, 7000)"            # only the 30- and 40-day-old logs
call PUT /settings '{"params":{"stale_days":null}}'; expect 200
call POST '/scan?view=summary'; expect 200
assert_field "d['matched']" 4

# --- 4. delete ------------------------------------------------------------------------
note "delete: preview, guard, then clean exactly what was reviewed"
call POST /clean '{"filter":{"ext":"log"},"dry_run":true}'; expect 200
assert_field "(d['would_delete']['matched'], d['would_delete']['bytes'])" "(2, 7000)"
[ -f "$TREE/junk/c.log" ] && [ -f "$TREE/junk/sub/d.log" ] || fail "dry_run deleted files"
call POST /clean '{"filter":{"ext":"log"},"expect_matched":3}'; expect 409
[ -f "$TREE/junk/c.log" ] || fail "a failed expect_* guard deleted files"
call POST /clean '{"filter":{"ext":"log"},"expect_matched":2,"expect_bytes":7000}'; expect 200
assert_field "(d['deleted'], d['bytes'], d['remaining'])" "(2, 7000, 2)"
[ ! -e "$TREE/junk/c.log" ] && [ ! -e "$TREE/junk/sub/d.log" ] || fail "logs were not deleted"
[ -f "$TREE/junk/a.tmp" ] && [ -f "$TREE/junk/b.tmp" ] || fail "unselected files were deleted"
call GET '/results?view=summary'; expect 200
assert_field "(d['total_matched'], d['total_bytes'])" "(2, 4000)"

note "delete: switch preset to the build-dirs rule and remove node_modules"
call PUT /settings '{"preset":"build-dirs"}'; expect 200
call POST /scan; expect 200
assert_field "(d['matched'], d['bytes'], d['matches'][0]['kind'])" "(1, 700, 'dir')"
call POST /clean; expect 200
assert_field "d['deleted']" 1
[ ! -e "$TREE/app/node_modules" ] || fail "node_modules was not deleted"

note "delete: back to the junk preset, clean everything it still matches"
call PUT /settings '{"preset":"junk"}'; expect 200
call POST '/scan?view=summary'; expect 200
assert_field "d['matched']" 2
call POST /clean; expect 200
assert_field "(d['deleted'], d['failed'], d['remaining'])" "(2, 0, 0)"
call POST /clean; expect 409                                     # nothing cached any more
for f in junk/fresh.tmp keep/readme.txt; do
  [ -f "$TREE/$f" ] || fail "$f must survive"
done
for f in junk/a.tmp junk/b.tmp; do
  [ ! -e "$TREE/$f" ] || fail "$f should be gone"
done

# --- 5. exit ----------------------------------------------------------------------------
note "shutdown and exit"
call POST /shutdown; expect 200
for _ in $(seq 150); do
  kill -0 "$SERVER_PID" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$SERVER_PID" 2>/dev/null; then fail "server still running after /shutdown"; fi
code=0; wait "$SERVER_PID" || code=$?
SERVER_PID=""
[ "$code" = 0 ] || fail "server exit code $code, want 0"
grep -q 'shutdown' "$WORK/server.out" || fail "no shutdown result in server output"

# --- 6. idle timeout exits by itself ---------------------------------------------------------
note "idle timeout: no requests, server exits on its own"
rm -f "$WORK/port"
"$LMSCRIPT_BIN" examples/clean-tmp.lms "$HOST_BIN" --serve \
  --idle-timeout 2 --port-file "$WORK/port" \
  > "$WORK/idle.out" 2> "$WORK/idle.err" &
SERVER_PID=$!
for _ in $(seq 150); do
  kill -0 "$SERVER_PID" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$SERVER_PID" 2>/dev/null; then fail "idle server did not exit within 30s"; fi
code=0; wait "$SERVER_PID" || code=$?
SERVER_PID=""
[ "$code" = 0 ] || fail "idle exit code $code, want 0"
grep -q 'idle' "$WORK/idle.out" || fail "idle shutdown not reported"

echo
echo "PASS: end-to-end server smoke test"
