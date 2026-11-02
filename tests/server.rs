//! Integration tests for `lmclean --serve`, driving the real release/debug
//! binary over real TCP with a hand-rolled HTTP client.
//!
//! Two groups:
//!  * protocol tests need no bytecode (lifecycle, settings, auth, errors,
//!    restore from a hand-written trash manifest);
//!  * example tests run each rule pack in `examples/` through the REST API and
//!    need compiled bytecode `NAME.dslb` for `clean-tmp`, `clean-build-dirs`,
//!    `clean-ignore` and `clean-stale-build-dirs` in `$LMCLEAN_EXAMPLES_DIR`
//!    (default `tests/fixtures`). Missing bytecode skips them locally; with
//!    `LMCLEAN_REQUIRE_EXAMPLES=1` (set in CI) a missing file is a failure.
#![allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use json::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

struct Server {
    child: Child,
    addr: String,
    stdout: BufReader<std::process::ChildStdout>,
    token: Option<String>,
}

impl Server {
    fn start(args: &[&str]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lmclean"))
            .arg("--serve")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lmclean --serve");
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read listening line");
        let doc = json::parse(line.trim()).unwrap_or_else(|e| panic!("bad startup line {line:?}: {e}"));
        let Some(Json::Str(addr)) = doc.get("listening") else {
            panic!("no listening address in {line:?}")
        };
        let token = args.iter().position(|a| *a == "--token").map(|i| args[i + 1].to_string());
        Server { child, addr: addr.clone(), stdout, token }
    }

    fn req(&self, method: &str, path: &str, body: &str) -> (u16, Json) {
        self.req_auth(method, path, body, self.token.as_deref())
    }

    fn req_auth(&self, method: &str, path: &str, body: &str, token: Option<&str>) -> (u16, Json) {
        let mut s = TcpStream::connect(&self.addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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

    /// Waits for the server to exit on its own; returns (exit code, remaining stdout).
    fn wait_exit(mut self, within: Duration) -> (i32, String) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut rest = String::new();
                self.stdout.read_to_string(&mut rest).unwrap();
                return (status.code().unwrap_or(-1), rest);
            }
            assert!(start.elapsed() < within, "server did not exit within {within:?}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
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

fn fixture(name: &str) -> Option<PathBuf> {
    let dir = std::env::var("LMCLEAN_EXAMPLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures"));
    let file = dir.join(format!("{name}.dslb"));
    if file.is_file() {
        return Some(std::fs::canonicalize(file).unwrap());
    }
    if std::env::var_os("LMCLEAN_REQUIRE_EXAMPLES").is_some_and(|v| v != "0" && !v.is_empty()) {
        panic!("{} is required (LMCLEAN_REQUIRE_EXAMPLES is set) but missing", file.display());
    }
    eprintln!("SKIP: {} not found; compile examples/{name}.lms to run this test", file.display());
    None
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

fn use_program(s: &Server, program: &Path, tree: &Path, extra: &str) -> Json {
    let body = format!("{{\"program\":{},\"paths\":[{}]{extra}}}", p(program), p(tree));
    let (status, d) = s.req("PUT", "/settings", &body);
    assert_eq!(status, 200, "{d:?}");
    d
}

#[test]
fn example_clean_tmp_scan_cache_and_clean() {
    let Some(prog) = fixture("clean-tmp") else { return };
    let tree = scratch("tmp").join("tree");
    build_tree(&tree);
    let s = Server::start(&["--idle-timeout", "0", "--allow-exec"]);
    use_program(&s, &prog, &tree, "");

    let (status, scan) = s.req("POST", "/scan", "");
    assert_eq!(status, 200, "{scan:?}");
    assert!(!boolean(&scan, "cached"));
    assert_eq!((num(&scan, "matched"), num(&scan, "bytes")), (3, 17));
    let want: Vec<String> = ["old.tmp", "stale.log", "sub/nested.tmp"]
        .iter()
        .map(|f| tree.join(f).to_str().unwrap().to_string())
        .collect();
    assert_eq!(matched_paths(&scan), want);
    assert!(tree.join("old.tmp").exists(), "scan must never delete");

    // The cache is served without walking again: delete a match behind the
    // server's back and the results are unchanged.
    std::fs::remove_file(tree.join("stale.log")).unwrap();
    let (status, cached) = s.req("GET", "/results", "");
    assert_eq!(status, 200);
    assert!(boolean(&cached, "cached"));
    assert_eq!(matched_paths(&cached), want);
    assert!(boolean(&s.req("GET", "/health", "").1, "cached"));

    // Changing a setting invalidates the cache.
    s.req("PUT", "/settings", r#"{"max_depth":0}"#);
    assert_eq!(s.req("GET", "/results", "").0, 404);
    let (_, rescan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&rescan), vec![tree.join("old.tmp").to_str().unwrap().to_string()],
        "max_depth 0 keeps only old.tmp (stale.log is gone, nested is too deep)");

    // Clean acts on the cached matches only.
    let (status, c) = s.req("POST", "/clean", "");
    assert_eq!(status, 200, "{c:?}");
    assert_eq!((num(&c, "deleted"), num(&c, "failed")), (1, 0));
    assert!(!tree.join("old.tmp").exists());
    for keep in ["keep.txt", "new.tmp", "sub/nested.tmp", ".hid/h.tmp"] {
        assert!(tree.join(keep).exists(), "{keep} must survive");
    }
    assert_eq!(s.req("POST", "/clean", "").0, 409, "cache is consumed by clean");
}

#[test]
fn example_clean_tmp_refuses_to_delete_without_allow_exec() {
    let Some(prog) = fixture("clean-tmp") else { return };
    let tree = scratch("tmp-noexec").join("tree");
    build_tree(&tree);
    let s = Server::start(&["--idle-timeout", "0"]);
    use_program(&s, &prog, &tree, "");
    assert_eq!(s.req("POST", "/scan", "").0, 200);
    assert_eq!(s.req("POST", "/clean", "").0, 403);
    assert!(tree.join("old.tmp").exists());
}

#[test]
fn example_clean_tmp_trash_then_restore() {
    let Some(prog) = fixture("clean-tmp") else { return };
    let base = scratch("tmp-trash");
    let tree = base.join("tree");
    let trash = base.join("trash");
    build_tree(&tree);
    let s = Server::start(&["--idle-timeout", "0", "--allow-exec"]);
    use_program(&s, &prog, &tree, &format!(",\"trash\":{}", p(&trash)));
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
    let Some(prog) = fixture("clean-tmp") else { return };
    let tree = scratch("tmp-hidden").join("tree");
    build_tree(&tree);
    let s = Server::start(&["--idle-timeout", "0"]);
    use_program(&s, &prog, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert!(!matched_paths(&scan).iter().any(|m| m.contains("h.tmp")));
    s.req("PUT", "/settings", r#"{"hidden":true}"#);
    let (_, scan) = s.req("POST", "/scan", "");
    assert!(matched_paths(&scan).iter().any(|m| m.ends_with("h.tmp")), "{scan:?}");
}

#[test]
fn example_clean_build_dirs() {
    let Some(prog) = fixture("clean-build-dirs") else { return };
    let tree = scratch("dirs").join("tree");
    build_tree(&tree);
    let s = Server::start(&["--idle-timeout", "0", "--allow-exec"]);
    use_program(&s, &prog, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tree.join("app/node_modules").to_str().unwrap().to_string()]);
    assert_eq!((num(&scan, "matched"), num(&scan, "bytes")), (1, 7));
    let Some(Json::Arr(m)) = scan.get("matches") else { panic!() };
    assert_eq!(m[0].get("kind"), Some(&Json::Str("dir".into())));
    assert_eq!(num(&m[0], "size"), 7);
    assert_eq!(num(&s.req("POST", "/clean", "").1, "deleted"), 1);
    assert!(!tree.join("app/node_modules").exists());
    assert!(tree.join("app/src/main.rs").exists());
}

#[test]
fn example_clean_ignore_prunes_vcs() {
    let Some(prog) = fixture("clean-ignore") else { return };
    let tree = scratch("ignore").join("tree");
    write(&tree.join(".git/hooks/x.tmp"), "vcs\n");
    age(&tree.join(".git/hooks/x.tmp"), 10);
    write(&tree.join("old.tmp"), "top\n");
    age(&tree.join("old.tmp"), 10);
    let s = Server::start(&["--idle-timeout", "0", "--allow-exec"]);
    use_program(&s, &prog, &tree, ",\"hidden\":true");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tree.join("old.tmp").to_str().unwrap().to_string()]);
    s.req("POST", "/clean", "");
    assert!(tree.join(".git/hooks/x.tmp").exists(), "@ignore must protect .git");
    assert!(!tree.join("old.tmp").exists());
}

#[test]
#[cfg_attr(not(unix), ignore = "directory mtimes cannot be set through std on Windows")]
fn example_clean_stale_build_dirs_24h() {
    let Some(prog) = fixture("clean-stale-build-dirs") else { return };
    let tree = scratch("stale").join("tree");
    std::fs::create_dir_all(tree.join("oldproj/target")).unwrap();
    std::fs::create_dir_all(tree.join("fresh/dist")).unwrap();
    std::fs::create_dir_all(tree.join("src/build")).unwrap();
    age(&tree.join("oldproj/target"), 2);
    let s = Server::start(&["--idle-timeout", "0"]);
    use_program(&s, &prog, &tree, "");
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tree.join("oldproj/target").to_str().unwrap().to_string()]);
    assert_eq!(num(&scan, "bytes"), 0);
}

#[test]
fn example_presets_switch_between_rule_packs() {
    let (Some(tmp), Some(dirs), Some(ignore), Some(stale)) = (
        fixture("clean-tmp"),
        fixture("clean-build-dirs"),
        fixture("clean-ignore"),
        fixture("clean-stale-build-dirs"),
    ) else {
        return;
    };
    let base = scratch("presets");
    let tree = base.join("tree");
    build_tree(&tree);
    let presets = format!(
        r#"{{"default":"tmp","presets":{{
            "tmp":{{"program":{},"paths":[{}]}},
            "build-dirs":{{"program":{},"paths":[{}],"max_depth":null}},
            "ignore":{{"program":{},"paths":[{}],"hidden":true}},
            "stale":{{"program":{},"paths":[{}]}}}}}}"#,
        p(&tmp), p(&tree), p(&dirs), p(&tree), p(&ignore), p(&tree), p(&stale), p(&tree)
    );
    write(&base.join("presets.json"), &presets);
    let file = base.join("presets.json");
    let s = Server::start(&["--idle-timeout", "0", "--presets", file.to_str().unwrap()]);

    let (_, list) = s.req("GET", "/presets", "");
    assert_eq!(list.get("default"), Some(&Json::Str("tmp".into())));
    assert_eq!(list.get("active"), Some(&Json::Str("tmp".into())));
    let Some(Json::Arr(items)) = list.get("presets") else { panic!("{list:?}") };
    assert_eq!(items.len(), 4);

    // Starts on the default preset with no further configuration.
    assert_eq!(num(&s.req("POST", "/scan", "").1, "matched"), 3);
    // Switching preset swaps program + settings and drops the cache.
    let (status, d) = s.req("PUT", "/settings", r#"{"preset":"build-dirs"}"#);
    assert_eq!(status, 200, "{d:?}");
    assert_eq!(d.get("preset"), Some(&Json::Str("build-dirs".into())));
    assert_eq!(s.req("GET", "/results", "").0, 404);
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(matched_paths(&scan), vec![tree.join("app/node_modules").to_str().unwrap().to_string()]);
    // A preset plus overrides, and an edit that makes the settings custom.
    s.req("PUT", "/settings", r#"{"preset":"ignore","max_depth":0}"#);
    let (_, d) = s.req("GET", "/settings", "");
    assert_eq!((d.get("preset"), num(&d, "max_depth")), (Some(&Json::Str("ignore".into())), 0));
    let (_, d) = s.req("PUT", "/settings", r#"{"hidden":false}"#);
    assert_eq!(d.get("preset"), Some(&Json::Null));
    let (_, scan) = s.req("POST", "/scan", "");
    assert_eq!(
        matched_paths(&scan),
        vec![tree.join("old.tmp").to_str().unwrap().to_string()],
        "clean-ignore at depth 0 without hidden: only the stale top-level .tmp"
    );
    // Presets without max_depth reset it.
    let (_, d) = s.req("PUT", "/settings", r#"{"preset":"stale"}"#);
    assert_eq!(d.get("max_depth"), Some(&Json::Null));
}
