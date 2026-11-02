//! Integration tests for `lmclean --serve`, driving the real binary over real
//! TCP with a hand-rolled HTTP client.
//!
//! Two groups:
//!  * protocol tests start the host directly and need no compiler;
//!  * example tests run each `examples/*.lms` rule pack through `lmscript`
//!    (which compiles it and launches the host via `run_lmcode`) and exercise
//!    the REST API against real compiled rules. They need `lmscript`
//!    (`$LMSCRIPT_BIN`, else `lmscript` on PATH); without it they skip
//!    locally, and with `LMCLEAN_REQUIRE_EXAMPLES=1` (CI) they fail instead.
#![allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use json::Json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

const HOST: &str = env!("CARGO_BIN_EXE_lmclean");
static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Server {
    child: Child,
    addr: String,
    token: Option<String>,
}

fn spawn_server(mut cmd: Command, serve_args: &[&str]) -> Server {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("server-test");
    std::fs::create_dir_all(&dir).unwrap();
    let port_file = dir.join(format!("port-{}-{n}", std::process::id()));
    let _ = std::fs::remove_file(&port_file);
    let mut child = cmd
        .args(serve_args)
        .arg("--port-file")
        .arg(&port_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let start = Instant::now();
    let addr = loop {
        if let Ok(a) = std::fs::read_to_string(&port_file) {
            if !a.is_empty() {
                break a;
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
            panic!("server exited early ({status}): {out}");
        }
        assert!(start.elapsed() < Duration::from_secs(60), "server never wrote its port file");
        std::thread::sleep(Duration::from_millis(25));
    };
    let token = serve_args.iter().position(|a| *a == "--token").map(|i| serve_args[i + 1].to_string());
    Server { child, addr, token }
}

impl Server {
    /// The host binary directly, no compiler involved.
    fn start(args: &[&str]) -> Server {
        let mut cmd = Command::new(HOST);
        cmd.arg("--serve");
        spawn_server(cmd, args)
    }

    /// An example rule pack served through `lmscript` / `run_lmcode`.
    fn example(script: &str, args: &[&str]) -> Server {
        let lmscript = lmscript_bin().expect("checked by caller");
        let mut cmd = Command::new(lmscript);
        cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(["--no-preflight", &format!("examples/{script}.lms"), HOST, "--serve"]);
        let mut all = vec!["--idle-timeout", "120"];
        all.extend_from_slice(args);
        spawn_server(cmd, &all)
    }

    fn req(&self, method: &str, path: &str, body: &str) -> (u16, Json) {
        self.req_auth(method, path, body, self.token.as_deref())
    }

    fn req_auth(&self, method: &str, path: &str, body: &str, token: Option<&str>) -> (u16, Json) {
        let mut s = TcpStream::connect(&self.addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).expect("read response");
        let (head, body) = raw.split_once("\r\n\r\n").expect("response head");
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let doc = json::parse(body).unwrap_or_else(|e| panic!("response is not JSON ({e}): {body:?}"));
        (status, doc)
    }

    /// Waits for the server to exit on its own; returns (exit code, stdout).
    fn wait_exit(mut self, within: Duration) -> (i32, String) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut out = String::new();
                self.child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
                return (status.code().unwrap_or(-1), out);
            }
            assert!(start.elapsed() < within, "server did not exit within {within:?}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Ask politely first: killing an lmscript parent would orphan the host.
        if let Ok(mut s) = TcpStream::connect(&self.addr) {
            let auth = self.token.as_ref().map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
            let _ = write!(s, "POST /shutdown HTTP/1.1\r\nHost: t\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = s.read_to_end(&mut Vec::new());
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn q(s: impl AsRef<str>) -> String {
    format!("\"{}\"", s.as_ref().replace('\\', "\\\\").replace('"', "\\\""))
}

fn p(path: &Path) -> String {
    q(path.to_str().unwrap())
}

fn num(doc: &Json, key: &str) -> i64 {
    match doc.get(key) {
        Some(Json::Num(n)) => *n as i64,
        other => panic!("{key} is not a number in {doc:?}: {other:?}"),
    }
}

fn boolean(doc: &Json, key: &str) -> bool {
    match doc.get(key) {
        Some(Json::Bool(b)) => *b,
        other => panic!("{key} is not a bool in {doc:?}: {other:?}"),
    }
}

fn matched_paths(doc: &Json) -> Vec<String> {
    let Some(Json::Arr(items)) = doc.get("matches") else { panic!("no matches in {doc:?}") };
    items
        .iter()
        .map(|m| match m.get("path") {
            Some(Json::Str(s)) => s.clone(),
            _ => panic!("match without path"),
        })
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("server-test")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn age(path: &Path, days: u64) {
    let when = SystemTime::now() - Duration::from_secs(days * 86400);
    // Files need write access on Windows; directories can only be opened
    // read-only (Unix), so fall back to that.
    let f = std::fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| std::fs::File::options().read(true).open(path))
        .unwrap();
    f.set_modified(when).unwrap();
}

// ---------------------------------------------------------------- protocol --

#[test]
fn lifecycle_health_and_shutdown() {
    let s = Server::start(&["--idle-timeout", "0"]);
    let (status, h) = s.req("GET", "/health", "");
    assert_eq!(status, 200);
    assert!(boolean(&h, "ok"));
    assert!(!boolean(&h, "cached"));
    assert!(!boolean(&h, "allow_exec"));
    assert_eq!(num(&h, "idle_timeout"), 0);
    let (status, b) = s.req("POST", "/shutdown", "");
    assert_eq!((status, boolean(&b, "shutdown")), (200, true));
    let (code, rest) = s.wait_exit(Duration::from_secs(5));
    assert_eq!(code, 0);
    assert!(rest.contains("\"shutdown\":\"requested\""), "{rest}");
}

#[test]
fn idle_timeout_stops_the_server() {
    let s = Server::start(&["--idle-timeout", "1"]);
    assert_eq!(s.req("GET", "/health", "").0, 200);
    let (code, rest) = s.wait_exit(Duration::from_secs(10));
    assert_eq!(code, 0);
    assert!(rest.contains("\"shutdown\":\"idle\""), "{rest}");
}

#[test]
fn activity_resets_the_idle_timer() {
    let s = Server::start(&["--idle-timeout", "2"]);
    let start = Instant::now();
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(s.req("GET", "/health", "").0, 200, "server died after {:?}", start.elapsed());
    }
    assert!(start.elapsed() > Duration::from_secs(3));
}

#[test]
fn routing_and_error_shapes() {
    let s = Server::start(&["--idle-timeout", "0"]);
    assert_eq!(s.req("GET", "/nope", "").0, 404);
    assert_eq!(s.req("DELETE", "/settings", "").0, 405);
    assert_eq!(s.req("GET", "/scan", "").0, 405);
    assert_eq!(s.req("GET", "/health/", "").0, 200, "trailing slash");
    assert_eq!(s.req("GET", "/health?x=1", "").0, 200, "query string ignored");
    let (status, e) = s.req("GET", "/results", "");
    assert_eq!(status, 404);
    assert!(!boolean(&e, "ok"));
    let (status, _) = s.req("POST", "/scan", "");
    assert_eq!(status, 409, "scan without a program");
    assert_eq!(s.req("POST", "/clean", "").0, 403, "clean without --allow-exec");
    assert_eq!(s.req("POST", "/restore", "").0, 403, "restore without --allow-exec");
}

#[test]
fn settings_roundtrip_and_validation() {
    let s = Server::start(&["--idle-timeout", "0"]);
    let (status, d) = s.req("GET", "/settings", "");
    assert_eq!(status, 200);
    assert_eq!((d.get("program"), d.get("hidden")), (Some(&Json::Null), Some(&Json::Bool(false))));
    let (status, d) = s.req("PUT", "/settings", r#"{"paths":["a","b"],"hidden":true,"max_depth":2,"trash":"t"}"#);
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(num(&d, "max_depth"), 2);
    assert!(boolean(&d, "hidden"));
    let (_, d) = s.req("PATCH", "/settings", r#"{"max_depth":null}"#);
    assert_eq!(d.get("max_depth"), Some(&Json::Null));
    assert_eq!(d.get("paths"), Some(&Json::Arr(vec![Json::Str("a".into()), Json::Str("b".into())])));
    for bad in [
        "not json",
        "[]",
        r#"{"bogus":1}"#,
        r#"{"hidden":"yes"}"#,
        r#"{"max_depth":-1}"#,
        r#"{"max_depth":1.5}"#,
        r#"{"paths":"a"}"#,
        r#"{"preset":"missing"}"#,
    ] {
        let (status, e) = s.req("PUT", "/settings", bad);
        assert!(status == 400 || status == 404, "{bad} -> {status}");
        assert!(!boolean(&e, "ok"));
    }
    // A failed update leaves settings untouched.
    let (_, d) = s.req("GET", "/settings", "");
    assert!(boolean(&d, "hidden"));
    // An unloadable program is rejected up front and does not stick.
    let (status, e) = s.req("PUT", "/settings", r#"{"program":"/definitely/not/here.dslb"}"#);
    assert_eq!(status, 400, "{e:?}");
    let (_, d) = s.req("GET", "/settings", "");
    assert_eq!(d.get("program"), Some(&Json::Null));
    // Garbage bytecode is a load error, not a crash.
    let dir = scratch("garbage");
    write(&dir.join("garbage.dslb"), "this is not bytecode");
    let body = format!("{{\"program\":{}}}", p(&dir.join("garbage.dslb")));
    assert_eq!(s.req("PUT", "/settings", &body).0, 400);
    assert_eq!(s.req("GET", "/health", "").0, 200, "server survives bad bytecode");
}

#[test]
fn bearer_token_is_enforced() {
    let s = Server::start(&["--idle-timeout", "0", "--token", "s3cret"]);
    assert_eq!(s.req_auth("GET", "/health", "", None).0, 401);
    assert_eq!(s.req_auth("GET", "/health", "", Some("wrong")).0, 401);
    assert_eq!(s.req_auth("POST", "/shutdown", "", Some("s3cret!")).0, 401);
    assert_eq!(s.req("GET", "/health", "").0, 200);
}

#[test]
fn non_loopback_listen_requires_a_token() {
    let out = Command::new(env!("CARGO_BIN_EXE_lmclean"))
        .args(["--serve", "--listen", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("--token"));
}

#[test]
fn serve_cli_errors_exit_2() {
    for args in [
        vec!["--bogus"],
        vec!["--idle-timeout", "soon"],
        vec!["--preset", "x"],
        vec!["--listen"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lmclean")).arg("--serve").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("\"phase\""), "{args:?}");
    }
}

#[test]
fn presets_file_errors_have_distinct_exit_codes() {
    let dir = scratch("badpresets");
    let run = |file: &str, extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lmclean"))
            .args(["--serve", "--presets"])
            .arg(dir.join(file))
            .args(extra)
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(run("missing.json", &[]), Some(4), "unreadable presets file");
    write(&dir.join("broken.json"), "{");
    assert_eq!(run("broken.json", &[]), Some(4), "malformed presets file");
    write(&dir.join("noprog.json"), r#"{"presets":{"a":{"paths":["."]}}}"#);
    assert_eq!(run("noprog.json", &[]), Some(4), "preset without program");
    write(&dir.join("unknown-key.json"), r#"{"presets":{"a":{"program":"x.dslb","wat":1}}}"#);
    assert_eq!(run("unknown-key.json", &[]), Some(4), "unknown preset key");
    write(&dir.join("ok.json"), r#"{"presets":{"a":{"program":"x.dslb"}}}"#);
    assert_eq!(run("ok.json", &["--preset", "zzz"]), Some(2), "unknown --preset");
    assert_eq!(run("ok.json", &["--preset", "a"]), Some(3), "start preset's program cannot be loaded");
}

#[test]
fn restore_replays_a_trash_manifest() {
    let dir = scratch("restore");
    let trash = dir.join("trash");
    let original = dir.join("tree").join("old.tmp");
    write(&trash.join("old.tmp"), "old\n");
    let manifest = format!("{{\"stored\":{},\"original\":{}}}\n", p(&trash.join("old.tmp")), p(&original));
    write(&trash.join("manifest.jsonl"), &manifest);

    let s = Server::start(&["--idle-timeout", "0", "--allow-exec"]);
    assert_eq!(s.req("POST", "/restore", "").0, 409, "no trash directory known");
    let (status, r) = s.req("POST", "/restore", &format!("{{\"trash\":{}}}", p(&trash)));
    assert_eq!(status, 200, "{r:?}");
    assert_eq!(num(&r, "restored"), 1);
    assert_eq!(std::fs::read_to_string(&original).unwrap(), "old\n");
    assert!(!trash.join("old.tmp").exists());
}

// ---------------------------------------------------------------- examples --

/// `lmscript` if it can be run; otherwise skip (or fail under REQUIRE).
fn lmscript_bin() -> Option<PathBuf> {
    let bin = PathBuf::from(std::env::var_os("LMSCRIPT_BIN").unwrap_or_else(|| "lmscript".into()));
    let runs = Command::new(&bin)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if runs {
        return Some(bin);
    }
    if std::env::var_os("LMCLEAN_REQUIRE_EXAMPLES").is_some_and(|v| v != "0" && !v.is_empty()) {
        panic!("lmscript ({}) is required (LMCLEAN_REQUIRE_EXAMPLES is set) but cannot run", bin.display());
    }
    eprintln!("SKIP: lmscript not available; set LMSCRIPT_BIN to run the example tests");
    None
}

macro_rules! need_lmscript {
    () => {
        if lmscript_bin().is_none() {
            return;
        }
    };
}

/// The tree test.sh uses, so numbers match the CLI gate.
fn build_tree(tree: &Path) {
    write(&tree.join("keep.txt"), "keep\n");
    write(&tree.join("old.tmp"), "old\n");
    age(&tree.join("old.tmp"), 10);
    write(&tree.join("stale.log"), "stale\n");
    age(&tree.join("stale.log"), 30);
    write(&tree.join("new.tmp"), "new\n");
    write(&tree.join("sub/nested.tmp"), "nested\n");
    age(&tree.join("sub/nested.tmp"), 10);
    write(&tree.join(".hid/h.tmp"), "hidden\n");
    age(&tree.join(".hid/h.tmp"), 10);
    write(&tree.join("app/node_modules/pkg/index.js"), "module\n");
    write(&tree.join("app/src/main.rs"), "code\n");
}

/// `tree/rel` as the server reports it (native separators on every platform).
fn tp(tree: &Path, rel: &str) -> String {
    rel.split('/').fold(tree.to_path_buf(), |p, c| p.join(c)).to_str().unwrap().to_string()
}

/// Points the server's search paths at `tree`.
fn use_tree(s: &Server, tree: &Path, extra: &str) {
    let (status, d) = s.req("PUT", "/settings", &format!("{{\"paths\":[{}]{extra}}}", p(tree)));
    assert_eq!(status, 200, "{d:?}");
}

#[test]
fn example_clean_tmp_scan_cache_and_clean() {
    need_lmscript!();
    let tree = scratch("tmp").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &["--allow-exec"]);
    use_tree(&s, &tree, "");

    let (status, scan) = s.req("POST", "/scan", "");
    assert_eq!(status, 200, "{scan:?}");
    assert!(!boolean(&scan, "cached"));
    assert_eq!((num(&scan, "matched"), num(&scan, "bytes")), (3, 17));
    let want = vec![tp(&tree, "old.tmp"), tp(&tree, "stale.log"), tp(&tree, "sub/nested.tmp")];
    let mut got = matched_paths(&scan);
    got.sort();
    assert_eq!(got, want);
    assert!(tree.join("old.tmp").exists(), "scan must never delete");

    // The cache is served without walking again: delete a match behind the
    // server's back and the results are unchanged.
    std::fs::remove_file(tree.join("stale.log")).unwrap();
    let (status, cached) = s.req("GET", "/results", "");
    assert_eq!(status, 200);
    assert!(boolean(&cached, "cached"));
    assert_eq!(num(&cached, "matched"), 3);
    assert!(boolean(&s.req("GET", "/health", "").1, "cached"));

    // Changing a setting invalidates the cache.
    s.req("PUT", "/settings", r#"{"max_depth":0}"#);
    assert_eq!(s.req("GET", "/results", "").0, 404);
    let (_, rescan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&rescan), vec![tp(&tree, "old.tmp")],
        "max_depth 0 keeps only old.tmp (stale.log is gone, nested is too deep)");

    let (status, c) = s.req("POST", "/clean", "");
    assert_eq!(status, 200, "{c:?}");
    assert_eq!((num(&c, "deleted"), num(&c, "failed"), num(&c, "remaining")), (1, 0, 0));
    assert!(!tree.join("old.tmp").exists());
    for keep in ["keep.txt", "new.tmp", "sub/nested.tmp", ".hid/h.tmp"] {
        assert!(tree.join(keep).exists(), "{keep} must survive");
    }
    assert_eq!(s.req("POST", "/clean", "").0, 409, "nothing cached once everything is cleaned");
}

#[test]
fn example_views_filter_sort_page_and_summarize() {
    need_lmscript!();
    let tree = scratch("views").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &[]);
    use_tree(&s, &tree, "");

    // Summary view: totals plus breakdowns, no match list.
    let (status, sum) = s.req("POST", "/scan?view=summary", "");
    assert_eq!(status, 200, "{sum:?}");
    assert!(sum.get("matches").is_none());
    assert_eq!((num(&sum, "total_matched"), num(&sum, "total_bytes")), (3, 17));
    let Some(Json::Arr(by_ext)) = sum.get("by_ext") else { panic!("{sum:?}") };
    assert_eq!(by_ext[0].get("ext"), Some(&Json::Str("tmp".into())), "ranked by bytes: tmp 11 > log 6");
    assert_eq!((num(&by_ext[0], "matched"), num(&by_ext[0], "bytes")), (2, 11));
    assert_eq!(num(sum.get("by_kind").unwrap().get("file").unwrap(), "bytes"), 17);

    let get = |query: &str| {
        let (status, d) = s.req("GET", &format!("/results?{query}"), "");
        assert_eq!(status, 200, "{query}: {d:?}");
        d
    };
    // Filters narrow the *selection*; the cache totals stay put.
    let d = get("ext=log");
    assert_eq!((num(&d, "matched"), num(&d, "bytes"), num(&d, "total_matched")), (1, 6, 3));
    assert_eq!(matched_paths(&d), vec![tp(&tree, "stale.log")]);
    assert_eq!(matched_paths(&get("ext=TMP,.log")).len(), 3, "extension list, case/dot insensitive");
    assert_eq!(matched_paths(&get("min_size=5")), vec![tp(&tree, "stale.log"), tp(&tree, "sub/nested.tmp")],
        "size filter; default sort is path ascending");
    assert_eq!(num(&get("max_size=5"), "matched"), 1);
    assert_eq!(matched_paths(&get("name=nested")), vec![tp(&tree, "sub/nested.tmp")]);
    assert_eq!(matched_paths(&get("path=sub")), vec![tp(&tree, "sub/nested.tmp")]);
    assert_eq!(matched_paths(&get("older_than_days=20")), vec![tp(&tree, "stale.log")]);
    assert_eq!(num(&get("newer_than_days=20"), "matched"), 2);
    assert_eq!(num(&get("kind=dir"), "matched"), 0);
    let d = get(&format!("exclude={}", tp(&tree, "old.tmp").replace('/', "%2F")));
    assert_eq!(num(&d, "matched"), 2, "percent-decoded exclude");
    // Sorting and paging.
    assert_eq!(matched_paths(&get("sort=size"))[0], tp(&tree, "sub/nested.tmp"), "size sorts biggest first");
    assert_eq!(matched_paths(&get("sort=size&order=asc"))[0], tp(&tree, "old.tmp"));
    let d = get("sort=size&limit=1&offset=1");
    assert_eq!((num(&d, "returned"), num(&d, "matched")), (1, 3));
    assert_eq!(matched_paths(&d), vec![tp(&tree, "stale.log")]);
    let item = &match d.get("matches") { Some(Json::Arr(a)) => a.clone(), _ => panic!() }[0];
    assert_eq!((num(item, "size"), num(item, "age_days")), (6, 30));
    // Bad queries are rejected, not ignored.
    for bad in ["bogus=1", "min_size=lots", "sort=color", "kind=pipe", "view=pretty", "order=up"] {
        assert_eq!(s.req("GET", &format!("/results?{bad}"), "").0, 400, "{bad}");
    }
}

#[test]
fn example_clean_selection_dry_run_and_expect_guard() {
    need_lmscript!();
    let tree = scratch("select").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &["--allow-exec"]);
    use_tree(&s, &tree, "");
    s.req("POST", "/scan", "");

    // Preview exactly what a filtered clean would remove.
    let (status, d) = s.req("POST", "/clean", r#"{"filter":{"ext":"tmp"},"dry_run":true}"#);
    assert_eq!(status, 200, "{d:?}");
    let w = d.get("would_delete").unwrap();
    assert_eq!((num(w, "matched"), num(w, "bytes")), (2, 11));
    assert!(tree.join("old.tmp").exists() && tree.join("sub/nested.tmp").exists(), "dry_run must not delete");

    // expect_* pins what the client reviewed; drift is a 409 and nothing happens.
    let (status, d) = s.req("POST", "/clean", r#"{"filter":{"ext":"tmp"},"expect_matched":3}"#);
    assert_eq!(status, 409, "{d:?}");
    assert_eq!((num(&d, "matched"), num(&d, "bytes")), (2, 11), "409 reports the actual selection");
    assert!(tree.join("old.tmp").exists());
    assert_eq!(s.req("POST", "/clean", r#"{"expect_bytes":999}"#).0, 409);

    // Clean only the reviewed selection; the rest stays cached for another round.
    let (status, c) = s.req("POST", "/clean", r#"{"filter":{"ext":"tmp"},"expect_matched":2,"expect_bytes":11}"#);
    assert_eq!(status, 200, "{c:?}");
    assert_eq!((num(&c, "selected"), num(&c, "deleted"), num(&c, "bytes"), num(&c, "remaining")), (2, 2, 11, 1));
    assert!(!tree.join("old.tmp").exists() && !tree.join("sub/nested.tmp").exists());
    assert!(tree.join("stale.log").exists());
    let (_, left) = s.req("GET", "/results", "");
    assert_eq!((num(&left, "total_matched"), num(&left, "total_bytes")), (1, 6));
    // Exclude protects individual files from an otherwise broad clean.
    let (status, d) = s.req("POST", "/clean", &format!(r#"{{"filter":{{"exclude":[{}]}}}}"#, p(&tree.join("stale.log"))));
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(num(&d, "selected"), 0);
    assert!(tree.join("stale.log").exists());
    // Bad bodies are rejected before anything runs.
    for bad in [r#"{"filter":{"color":1}}"#, r#"{"nope":1}"#, r#"{"dry_run":"yes"}"#, r#"{"expect_bytes":-1}"#, "[]"] {
        assert_eq!(s.req("POST", "/clean", bad).0, 400, "{bad}");
    }
    let (status, _) = s.req("POST", "/clean", "");
    assert_eq!(status, 200, "no filter cleans everything left");
    assert!(!tree.join("stale.log").exists());
}

#[test]
fn example_dry_run_clean_works_without_allow_exec() {
    need_lmscript!();
    let tree = scratch("noexec").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &[]);
    use_tree(&s, &tree, "");
    assert_eq!(s.req("POST", "/scan", "").0, 200);
    let (status, d) = s.req("POST", "/clean", r#"{"dry_run":true}"#);
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(num(d.get("would_delete").unwrap(), "matched"), 3);
    assert_eq!(s.req("POST", "/clean", "").0, 403, "a real clean needs --allow-exec");
    assert!(tree.join("old.tmp").exists());
}

#[test]
fn example_params_tune_the_rule_itself() {
    need_lmscript!();
    let tree = scratch("params").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &[]);
    use_tree(&s, &tree, "");
    let scan = |expect: i64| {
        let (status, d) = s.req("POST", "/scan?view=summary", "");
        assert_eq!(status, 200, "{d:?}");
        assert_eq!(num(&d, "matched"), expect, "{d:?}");
    };
    scan(3);
    // Rule parameters live in settings and re-parameterize the compiled rule.
    let (status, d) = s.req("PUT", "/settings", r#"{"params":{"stale_days":20}}"#);
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(d.get("params").unwrap().get("stale_days"), Some(&Json::Str("20".into())));
    assert_eq!(s.req("GET", "/results", "").0, 404, "changing params drops the cache");
    scan(1); // only stale.log is 30 days old
    s.req("PUT", "/settings", r#"{"params":{"stale_days":null,"exts":"tmp"}}"#);
    scan(2); // back to 7 days, but only *.tmp
    s.req("PUT", "/settings", r#"{"params":{"stale_days":-1,"exts":"tmp,log"}}"#);
    scan(4); // "older than -1 days" is everything: old.tmp, stale.log, new.tmp, sub/nested.tmp
    // A parameter the rule cannot use is a run error, and the server survives.
    s.req("PUT", "/settings", r#"{"params":{"stale_days":"soon"}}"#);
    let (status, e) = s.req("POST", "/scan", "");
    assert_eq!(status, 500, "{e:?}");
    assert_eq!(s.req("GET", "/health", "").0, 200);
    assert_eq!(s.req("PUT", "/settings", r#"{"params":{"x":[1]}}"#).0, 400);
    assert_eq!(s.req("PUT", "/settings", r#"{"params":5}"#).0, 400);
}

#[test]
fn example_clean_tmp_trash_then_restore() {
    need_lmscript!();
    let base = scratch("tmp-trash");
    let tree = base.join("tree");
    let trash = base.join("trash");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &["--allow-exec"]);
    use_tree(&s, &tree, &format!(",\"trash\":{}", p(&trash)));
    s.req("POST", "/scan", "");
    let (status, c) = s.req("POST", "/clean", "");
    assert_eq!(status, 200, "{c:?}");
    assert_eq!(c.get("mode"), Some(&Json::Str("trash".into())));
    assert_eq!(num(&c, "deleted"), 3);
    assert!(trash.join("sub/nested.tmp").exists());
    assert!(!tree.join("old.tmp").exists());
    let (status, r) = s.req("POST", "/restore", "");
    assert_eq!(status, 200, "{r:?}");
    assert_eq!(num(&r, "restored"), 3);
    assert_eq!(std::fs::read_to_string(tree.join("old.tmp")).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(tree.join("sub/nested.tmp")).unwrap(), "nested\n");
}

#[test]
fn example_clean_tmp_hidden_toggle() {
    need_lmscript!();
    let tree = scratch("tmp-hidden").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-tmp", &[]);
    use_tree(&s, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert!(!matched_paths(&scan).iter().any(|m| m.contains("h.tmp")));
    s.req("PUT", "/settings", r#"{"hidden":true}"#);
    let (_, scan) = s.req("POST", "/scan", "");
    assert!(matched_paths(&scan).iter().any(|m| m.ends_with("h.tmp")), "{scan:?}");
}

#[test]
fn example_clean_build_dirs() {
    need_lmscript!();
    let tree = scratch("dirs").join("tree");
    build_tree(&tree);
    let s = Server::example("clean-build-dirs", &["--allow-exec"]);
    use_tree(&s, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "app/node_modules")]);
    assert_eq!((num(&scan, "matched"), num(&scan, "bytes")), (1, 7));
    let Some(Json::Arr(m)) = scan.get("matches") else { panic!() };
    assert_eq!(m[0].get("kind"), Some(&Json::Str("dir".into())));
    assert_eq!(num(&m[0], "size"), 7);
    assert_eq!(num(&s.req("GET", "/results?kind=file", "").1, "matched"), 0);
    // The directory names are a rule parameter.
    s.req("PUT", "/settings", r#"{"params":{"names":"src"}}"#);
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "app/src")]);
    s.req("PUT", "/settings", r#"{"params":{"names":null}}"#);
    s.req("POST", "/scan", "");
    assert_eq!(num(&s.req("POST", "/clean", "").1, "deleted"), 1);
    assert!(!tree.join("app/node_modules").exists());
    assert!(tree.join("app/src/main.rs").exists());
}

#[test]
fn example_clean_ignore_prunes_vcs() {
    need_lmscript!();
    let tree = scratch("ignore").join("tree");
    write(&tree.join(".git/hooks/x.tmp"), "vcs\n");
    age(&tree.join(".git/hooks/x.tmp"), 10);
    write(&tree.join("old.tmp"), "top\n");
    age(&tree.join("old.tmp"), 10);
    let s = Server::example("clean-ignore", &["--allow-exec"]);
    use_tree(&s, &tree, ",\"hidden\":true");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "old.tmp")]);
    // The prune list is a rule parameter: stop pruning and .git is fair game.
    s.req("PUT", "/settings", r#"{"params":{"ignore":"none"}}"#);
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(num(&scan, "matched"), 2, "{scan:?}");
    s.req("PUT", "/settings", r#"{"params":{"ignore":null}}"#);
    s.req("POST", "/scan", "");
    s.req("POST", "/clean", "");
    assert!(tree.join(".git/hooks/x.tmp").exists(), "@ignore must protect .git");
    assert!(!tree.join("old.tmp").exists());
}

#[test]
#[cfg_attr(not(unix), ignore = "directory mtimes cannot be set through std on Windows")]
fn example_clean_stale_build_dirs() {
    need_lmscript!();
    let tree = scratch("stale").join("tree");
    std::fs::create_dir_all(tree.join("oldproj/target")).unwrap();
    std::fs::create_dir_all(tree.join("fresh/dist")).unwrap();
    std::fs::create_dir_all(tree.join("src/build")).unwrap();
    age(&tree.join("oldproj/target"), 2);
    let s = Server::example("clean-stale-build-dirs", &[]);
    use_tree(&s, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "oldproj/target")]);
    assert_eq!(num(&scan, "bytes"), 0);
    s.req("PUT", "/settings", r#"{"params":{"stale_hours":72}}"#);
    assert_eq!(num(&s.req("POST", "/scan", "").1, "matched"), 0, "2 days is not older than 72h");
}

#[test]
fn example_presets_switch_between_all_rule_packs() {
    need_lmscript!();
    let base = scratch("presets");
    let tree = base.join("tree");
    build_tree(&tree);
    let t = p(&tree);
    // One server, four compiled programs (builder slots 0..3), four presets.
    let presets = format!(
        r#"{{"default":"tmp","presets":{{
            "tmp":{{"program":0,"paths":[{t}],"params":{{"stale_days":7}}}},
            "build-dirs":{{"program":1,"paths":[{t}],"max_depth":null}},
            "ignore":{{"program":2,"paths":[{t}],"hidden":true}},
            "stale":{{"program":3,"paths":[{t}]}}}}}}"#
    );
    write(&base.join("presets.json"), &presets);
    let file = base.join("presets.json");
    let lmscript = lmscript_bin().unwrap();
    let mut cmd = Command::new(lmscript);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR")).args(["--no-preflight", "examples/serve-all.lms", HOST, "--serve"]);
    let s = spawn_server(cmd, &["--idle-timeout", "120", "--presets", file.to_str().unwrap()]);

    let (_, list) = s.req("GET", "/presets", "");
    assert_eq!(list.get("default"), Some(&Json::Str("tmp".into())));
    assert_eq!(list.get("active"), Some(&Json::Str("tmp".into())));
    let Some(Json::Arr(items)) = list.get("presets") else { panic!("{list:?}") };
    assert_eq!(items.len(), 4);

    // Starts on the default preset with no further configuration.
    assert_eq!(num(&s.req("POST", "/scan?view=summary", "").1, "matched"), 3);
    // Switching preset swaps program + settings and drops the cache.
    let (status, d) = s.req("PUT", "/settings", r#"{"preset":"build-dirs"}"#);
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(d.get("preset"), Some(&Json::Str("build-dirs".into())));
    assert_eq!(s.req("GET", "/results", "").0, 404);
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "app/node_modules")]);
    // A preset plus overrides; an edit without naming a preset makes it custom.
    s.req("PUT", "/settings", r#"{"preset":"ignore","max_depth":0}"#);
    let (_, d) = s.req("GET", "/settings", "");
    assert_eq!((d.get("preset"), num(&d, "max_depth")), (Some(&Json::Str("ignore".into())), 0));
    let (_, d) = s.req("PUT", "/settings", r#"{"hidden":false}"#);
    assert_eq!(d.get("preset"), Some(&Json::Null));
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tp(&tree, "old.tmp")],
        "clean-ignore at depth 0, hidden off: only the stale top-level .tmp");
    // Presets without max_depth reset it; a bad slot is rejected.
    let (_, d) = s.req("PUT", "/settings", r#"{"preset":"stale"}"#);
    assert_eq!(d.get("max_depth"), Some(&Json::Null));
    assert_eq!(s.req("PUT", "/settings", r#"{"program":9}"#).0, 400);
    assert_eq!(s.req("GET", "/health", "").0, 200);
    // Shutting down the builder-driven server completes run_lmcode cleanly.
    assert_eq!(s.req("POST", "/shutdown", "").0, 200);
    let (code, _) = s.wait_exit(Duration::from_secs(30));
    assert_eq!(code, 0);
}
