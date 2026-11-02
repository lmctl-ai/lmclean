//! `lmclean --serve`: a small single-threaded HTTP/1.1 + JSON server around the
//! same rule engine the CLI uses. std only, one connection at a time, every
//! response is `Connection: close`.
//!
//! Lifecycle: it prints `{"listening":"ADDR"}` on stdout once bound, serves
//! until `POST /shutdown` or until no request has arrived for `--idle-timeout`
//! seconds, then prints `{"shutdown":"requested|idle"}` and exits 0.
//!
//! Model: the server holds *settings* (rule program, search paths, hidden,
//! max-depth, trash dir), optionally seeded from a named *preset*. `POST /scan`
//! walks and caches the matches (the intermediate result); `GET /results`
//! re-serves the cache without walking; `POST /clean` acts on exactly the
//! cached matches. Changing any setting drops the cache. Deleting is refused
//! unless the server was started with `--allow-exec`, so the dry-run-by-default
//! contract holds over the wire too.
use crate::host::{self, write_json_string};
use crate::json::{self, Json};
use crate::{trash, walk};
use dslvm::{error_json, report_app_failure, report_error, Error, Program, Value};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SERVE_USAGE: &str = "usage: --serve [--listen ADDR:PORT] [--presets FILE] [--preset NAME] [--idle-timeout SECS] [--allow-exec] [--token TOKEN]";
const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 16 << 10;

pub struct Config {
    listen: String,
    presets: Option<PathBuf>,
    preset: Option<String>,
    idle: Option<Duration>,
    allow_exec: bool,
    token: Option<String>,
}

pub fn parse_args(args: &[std::ffi::OsString]) -> Result<Config, String> {
    let mut cfg = Config {
        listen: "127.0.0.1:0".into(),
        presets: None,
        preset: None,
        idle: Some(Duration::from_secs(300)),
        allow_exec: false,
        token: None,
    };
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .and_then(|v| v.to_str())
            .map(str::to_string)
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while i < args.len() {
        match args[i].to_str() {
            Some("--listen") => cfg.listen = value(&mut i, "--listen")?,
            Some("--presets") => cfg.presets = Some(PathBuf::from(value(&mut i, "--presets")?)),
            Some("--preset") => cfg.preset = Some(value(&mut i, "--preset")?),
            Some("--token") => cfg.token = Some(value(&mut i, "--token")?),
            Some("--allow-exec") => cfg.allow_exec = true,
            Some("--idle-timeout") => {
                let secs: u64 = value(&mut i, "--idle-timeout")?
                    .parse()
                    .map_err(|_| "--idle-timeout needs a nonnegative integer of seconds".to_string())?;
                cfg.idle = (secs > 0).then(|| Duration::from_secs(secs));
            }
            Some(other) => return Err(format!("unknown serve argument {other}")),
            None => return Err("arguments must be UTF-8".into()),
        }
        i += 1;
    }
    if cfg.preset.is_some() && cfg.presets.is_none() {
        return Err("--preset requires --presets FILE".into());
    }
    Ok(cfg)
}

#[derive(Clone, Default)]
struct Settings {
    program: Option<PathBuf>,
    paths: Vec<PathBuf>,
    hidden: bool,
    max_depth: Option<u32>,
    trash: Option<PathBuf>,
}

struct Presets {
    default: Option<String>,
    items: Vec<(String, Settings)>,
}

fn str_field<'a>(key: &str, v: &'a Json) -> Result<&'a str, String> {
    match v {
        Json::Str(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("\"{key}\" must be a non-empty string")),
    }
}

/// Applies the keys of `obj` onto `s`. `base` resolves relative paths (the
/// presets file's directory); keys in `skip` are ignored (the PUT `preset` key).
fn apply_fields(s: &mut Settings, obj: &Json, base: Option<&Path>, skip: &[&str]) -> Result<(), String> {
    let Json::Obj(items) = obj else {
        return Err("expected a JSON object".into());
    };
    let resolve = |p: &str| match base {
        Some(b) => b.join(p),
        None => PathBuf::from(p),
    };
    for (key, v) in items {
        match key.as_str() {
            k if skip.contains(&k) => {}
            "program" => s.program = Some(resolve(str_field(key, v)?)),
            "paths" => {
                let Json::Arr(list) = v else {
                    return Err("\"paths\" must be an array of strings".into());
                };
                s.paths = list
                    .iter()
                    .map(|p| str_field("paths[]", p).map(&resolve))
                    .collect::<Result<_, _>>()?;
            }
            "hidden" => match v {
                Json::Bool(b) => s.hidden = *b,
                _ => return Err("\"hidden\" must be a boolean".into()),
            },
            "max_depth" => match v {
                Json::Null => s.max_depth = None,
                Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n <= f64::from(u32::MAX) => {
                    s.max_depth = Some(*n as u32)
                }
                _ => return Err("\"max_depth\" must be a nonnegative integer or null".into()),
            },
            "trash" => match v {
                Json::Null => s.trash = None,
                _ => s.trash = Some(resolve(str_field(key, v)?)),
            },
            other => return Err(format!("unknown setting \"{other}\"")),
        }
    }
    Ok(())
}

fn load_presets(file: &Path) -> Result<Presets, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("presets {}: {e}", file.display()))?;
    let doc = json::parse(&text).map_err(|e| format!("presets {}: {e}", file.display()))?;
    let base = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Some(Json::Obj(map)) = doc.get("presets") else {
        return Err("presets file needs a \"presets\" object".into());
    };
    let mut items = Vec::new();
    for (name, body) in map {
        let mut s = Settings::default();
        apply_fields(&mut s, body, Some(base), &[]).map_err(|e| format!("preset \"{name}\": {e}"))?;
        if s.program.is_none() {
            return Err(format!("preset \"{name}\": \"program\" is required"));
        }
        items.push((name.clone(), s));
    }
    let default = match doc.get("default") {
        None | Some(Json::Null) => None,
        Some(Json::Str(d)) if items.iter().any(|(n, _)| n == d) => Some(d.clone()),
        Some(_) => return Err("\"default\" must name an existing preset".into()),
    };
    for key in match &doc {
        Json::Obj(o) => o.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        _ => vec![],
    } {
        if key != "presets" && key != "default" {
            return Err(format!("unknown presets-file key \"{key}\""));
        }
    }
    Ok(Presets { default, items })
}

struct Match {
    path: PathBuf,
    root: PathBuf,
    size: i64,
    kind: &'static str,
}

struct Cache {
    at: u64,
    matches: Vec<Match>,
    bytes: u64,
}

struct State {
    cfg: Config,
    presets: Presets,
    active: Option<String>,
    settings: Settings,
    program: Option<Program>,
    host: host::Host,
    cache: Option<Cache>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_program(path: &Path, host: &host::Host) -> Result<Program, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Program::load(&bytes, crate::SCHEMA, &host.registry).map_err(|e| {
        let why = match e.code.as_str() {
            "LOAD_SCHEMA_MISMATCH" => "lmclean schema mismatch",
            "LOAD_UNBOUND" => "lmclean import mismatch",
            _ => "invalid lmclean bytecode",
        };
        format!("{}: {why} ({})", path.display(), e.code)
    })
}

/// Entry indexes by role, with the same interface checks as the CLI.
struct Roles {
    ignores: Vec<usize>,
    selects: Vec<usize>,
    reports: Vec<usize>,
}

fn classify(program: &Program) -> Result<Roles, String> {
    let mut r = Roles { ignores: vec![], selects: vec![], reports: vec![] };
    for (index, entry) in program.entries().iter().enumerate() {
        let unary_file = entry.params.as_slice() == ["File"];
        let (list, ok, shape) = match entry.annotation.as_str() {
            "select" => (&mut r.selects, entry.returns == "bool", "Filter(File) -> bool"),
            "report" => (&mut r.reports, entry.returns == "void", "Report(File) -> void"),
            "ignore" => (&mut r.ignores, entry.returns == "bool", "Ignore(File) -> bool"),
            _ => continue,
        };
        if !(unary_file && ok) {
            return Err(format!(
                "entry '{}' annotated @{} must implement {shape}",
                entry.name, entry.annotation
            ));
        }
        list.push(index);
    }
    Ok(r)
}

fn scan(program: &Program, host: &mut host::Host, s: &Settings) -> Result<Cache, String> {
    program.run(&mut host.context, &host.registry).map_err(|e| e.message)?;
    let Roles { ignores, selects, reports } = classify(program)?;
    let roots: Vec<PathBuf> = if s.paths.is_empty() { vec![PathBuf::from(".")] } else { s.paths.clone() };
    let opts = walk::Options { hidden: s.hidden, max_depth: s.max_depth };
    let (mut matches, mut bytes) = (Vec::new(), 0u64);
    for root in &roots {
        walk::walk(root, &opts, &mut |cand| {
            let obj = host.context.insert("File", crate::file_of(cand));
            let outcome = (|| -> Result<bool, Error> {
                let arg = Value::object("File", obj);
                for &i in &ignores {
                    if program.call(i, &mut host.context, &host.registry, vec![arg.clone()])? == Value::Bool(true) {
                        return Ok(false);
                    }
                }
                let mut hit = false;
                for &i in &selects {
                    if program.call(i, &mut host.context, &host.registry, vec![arg.clone()])? == Value::Bool(true) {
                        hit = true;
                    }
                }
                if !hit {
                    return Ok(true);
                }
                for &i in &reports {
                    program.call(i, &mut host.context, &host.registry, vec![arg.clone()])?;
                }
                let size = walk::rsize(&cand.path);
                bytes += size as u64;
                let kind = if cand.metadata.is_dir() {
                    "dir"
                } else if cand.metadata.is_symlink() {
                    "symlink"
                } else {
                    "file"
                };
                matches.push(Match { path: cand.path.clone(), root: root.clone(), size, kind });
                Ok(false)
            })();
            let _ = host.context.take::<host::File>(obj);
            outcome
        })
        .map_err(|e| e.message)?;
    }
    Ok(Cache { at: now_secs(), matches, bytes })
}

// ---- JSON output helpers (byte-exact path escaping) ------------------------

fn jstr(out: &mut Vec<u8>, bytes: &[u8]) {
    let _ = write_json_string(out, bytes);
}

fn jopt_path(out: &mut Vec<u8>, p: &Option<PathBuf>) {
    match p {
        Some(p) => jstr(out, &walk::path_bytes(p)),
        None => out.extend_from_slice(b"null"),
    }
}

struct Resp {
    status: u16,
    body: Vec<u8>,
}

fn ok(body: Vec<u8>) -> Resp {
    Resp { status: 200, body }
}

fn fail(status: u16, message: &str) -> Resp {
    let mut body = b"{\"ok\":false,\"error\":".to_vec();
    jstr(&mut body, message.as_bytes());
    body.push(b'}');
    Resp { status, body }
}

impl State {
    fn settings_json(&self) -> Vec<u8> {
        let s = &self.settings;
        let mut o = b"{\"ok\":true,\"preset\":".to_vec();
        match &self.active {
            Some(n) => jstr(&mut o, n.as_bytes()),
            None => o.extend_from_slice(b"null"),
        }
        o.extend_from_slice(b",\"program\":");
        jopt_path(&mut o, &s.program);
        o.extend_from_slice(b",\"paths\":[");
        for (i, p) in s.paths.iter().enumerate() {
            if i > 0 {
                o.push(b',');
            }
            jstr(&mut o, &walk::path_bytes(p));
        }
        o.extend_from_slice(format!("],\"hidden\":{},\"max_depth\":", s.hidden).as_bytes());
        match s.max_depth {
            Some(n) => o.extend_from_slice(n.to_string().as_bytes()),
            None => o.extend_from_slice(b"null"),
        }
        o.extend_from_slice(b",\"trash\":");
        jopt_path(&mut o, &s.trash);
        o.extend_from_slice(format!(",\"allow_exec\":{}}}", self.cfg.allow_exec).as_bytes());
        o
    }

    fn results_json(&self, cached: bool) -> Option<Vec<u8>> {
        let c = self.cache.as_ref()?;
        let mut o = format!(
            "{{\"ok\":true,\"cached\":{cached},\"scanned_at\":{},\"matched\":{},\"bytes\":{},\"matches\":[",
            c.at,
            c.matches.len(),
            c.bytes
        )
        .into_bytes();
        for (i, m) in c.matches.iter().enumerate() {
            if i > 0 {
                o.push(b',');
            }
            o.extend_from_slice(b"{\"path\":");
            jstr(&mut o, &walk::path_bytes(&m.path));
            o.extend_from_slice(format!(",\"size\":{},\"kind\":\"{}\"}}", m.size, m.kind).as_bytes());
        }
        o.extend_from_slice(b"]}");
        Some(o)
    }

    /// Validates `next` completely, then commits it and drops the cache.
    fn commit(&mut self, next: Settings, active: Option<String>) -> Result<(), String> {
        if next.program != self.settings.program || self.program.is_none() {
            self.program = match &next.program {
                Some(p) => {
                    let program = load_program(p, &self.host)?;
                    classify(&program)?;
                    Some(program)
                }
                None => None,
            };
        }
        self.settings = next;
        self.active = active;
        self.cache = None;
        Ok(())
    }

    fn presets_json(&self) -> Vec<u8> {
        let mut o = b"{\"ok\":true,\"active\":".to_vec();
        match &self.active {
            Some(n) => jstr(&mut o, n.as_bytes()),
            None => o.extend_from_slice(b"null"),
        }
        o.extend_from_slice(b",\"default\":");
        match &self.presets.default {
            Some(n) => jstr(&mut o, n.as_bytes()),
            None => o.extend_from_slice(b"null"),
        }
        o.extend_from_slice(b",\"presets\":[");
        for (i, (name, s)) in self.presets.items.iter().enumerate() {
            if i > 0 {
                o.push(b',');
            }
            o.extend_from_slice(b"{\"name\":");
            jstr(&mut o, name.as_bytes());
            o.extend_from_slice(b",\"program\":");
            jopt_path(&mut o, &s.program);
            o.extend_from_slice(b",\"paths\":[");
            for (j, p) in s.paths.iter().enumerate() {
                if j > 0 {
                    o.push(b',');
                }
                jstr(&mut o, &walk::path_bytes(p));
            }
            o.extend_from_slice(format!("],\"hidden\":{}}}", s.hidden).as_bytes());
        }
        o.extend_from_slice(b"]}");
        o
    }

    fn put_settings(&mut self, body: &[u8]) -> Resp {
        let doc = match std::str::from_utf8(body).map_err(|e| e.to_string()).and_then(json::parse) {
            Ok(d @ Json::Obj(_)) => d,
            Ok(_) => return fail(400, "body must be a JSON object"),
            Err(e) => return fail(400, &format!("invalid JSON: {e}")),
        };
        let mut next = self.settings.clone();
        let mut active = self.active.clone();
        if let Some(name) = doc.get("preset") {
            let Json::Str(name) = name else {
                return fail(400, "\"preset\" must be a string");
            };
            match self.presets.items.iter().find(|(n, _)| n == name) {
                Some((_, s)) => {
                    next = s.clone();
                    active = Some(name.clone());
                }
                None => return fail(404, &format!("no such preset \"{name}\"")),
            }
        }
        if let Err(e) = apply_fields(&mut next, &doc, None, &["preset"]) {
            return fail(400, &e);
        }
        // Editing fields without naming a preset makes the settings custom.
        if doc.get("preset").is_none() {
            active = None;
        }
        match self.commit(next, active) {
            Ok(()) => ok(self.settings_json()),
            Err(e) => fail(400, &e),
        }
    }

    fn scan(&mut self) -> Resp {
        let Some(program) = &self.program else {
            return fail(409, "no program configured: choose a preset or PUT /settings with \"program\"");
        };
        match scan(program, &mut self.host, &self.settings) {
            Ok(cache) => {
                self.cache = Some(cache);
                ok(self.results_json(false).unwrap_or_default())
            }
            Err(e) => fail(500, &e),
        }
    }

    fn clean(&mut self) -> Resp {
        if !self.cfg.allow_exec {
            return fail(403, "deleting is disabled: start the server with --allow-exec");
        }
        let Some(cache) = self.cache.take() else {
            return fail(409, "no cached scan: POST /scan first");
        };
        let mut sink = match &self.settings.trash {
            Some(dir) => match trash::Trash::open(dir) {
                Ok(t) => Some(t),
                Err(e) => {
                    self.cache = Some(cache);
                    return fail(500, &format!("trash: {e}"));
                }
            },
            None => None,
        };
        let (mut deleted, mut missing, mut failed, mut bytes) = (0u64, 0u64, 0u64, 0u64);
        for m in &cache.matches {
            let meta = match std::fs::symlink_metadata(&m.path) {
                Ok(meta) => meta,
                Err(_) => {
                    missing += 1;
                    continue;
                }
            };
            let result = match sink.as_mut() {
                Some(t) => t.store(&m.path, &m.root).map(|_| ()),
                None if meta.is_dir() => std::fs::remove_dir_all(&m.path),
                None => std::fs::remove_file(&m.path),
            };
            match result {
                Ok(()) => {
                    deleted += 1;
                    bytes += m.size as u64;
                }
                Err(e) => {
                    eprintln!("lmclean: cannot remove {}: {e}", m.path.display());
                    failed += 1;
                }
            }
        }
        let mode = if sink.is_some() { "trash" } else { "unlink" };
        let body = format!(
            "{{\"ok\":{},\"mode\":\"{mode}\",\"deleted\":{deleted},\"missing\":{missing},\"failed\":{failed},\"bytes\":{bytes}}}",
            failed == 0
        )
        .into_bytes();
        Resp { status: if failed == 0 { 200 } else { 500 }, body }
    }

    fn restore(&mut self, body: &[u8]) -> Resp {
        if !self.cfg.allow_exec {
            return fail(403, "restoring is disabled: start the server with --allow-exec");
        }
        let mut dir = self.settings.trash.clone();
        if !body.iter().all(u8::is_ascii_whitespace) {
            match std::str::from_utf8(body).map_err(|e| e.to_string()).and_then(json::parse) {
                Ok(doc @ Json::Obj(_)) => match doc.get("trash") {
                    Some(Json::Str(d)) if !d.is_empty() => dir = Some(PathBuf::from(d)),
                    None => {}
                    Some(_) => return fail(400, "\"trash\" must be a non-empty string"),
                },
                Ok(_) => return fail(400, "body must be a JSON object"),
                Err(e) => return fail(400, &format!("invalid JSON: {e}")),
            }
        }
        let Some(dir) = dir else {
            return fail(409, "no trash directory: set \"trash\" in settings or in the body");
        };
        let mut sink = Vec::new();
        match trash::restore(&dir, &mut sink) {
            Ok(n) => {
                self.cache = None;
                ok(format!("{{\"ok\":true,\"restored\":{n}}}").into_bytes())
            }
            Err(e) => fail(500, &format!("restore: {e}")),
        }
    }
}

// ---- HTTP -------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, Resp> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return Err(fail(431, "request head too large"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(fail(400, "incomplete request")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Err(fail(408, "request timed out")),
        }
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| fail(400, "request head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if method.is_empty() || !target.starts_with('/') {
        return Err(fail(400, "malformed request line"));
    }
    let (mut length, mut auth) = (0usize, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(fail(400, "malformed header"));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().map_err(|_| fail(400, "bad content-length"))?,
            "authorization" => auth = Some(value.to_string()),
            "transfer-encoding" => return Err(fail(501, "transfer-encoding is not supported")),
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err(fail(413, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(fail(400, "incomplete body")),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(length);
    let path = target.split(['?', '#']).next().unwrap_or("/").trim_end_matches('/');
    Ok(Request {
        method: method.to_string(),
        path: if path.is_empty() { "/".into() } else { path.into() },
        auth,
        body,
    })
}

fn write_response(stream: &mut TcpStream, r: &Resp) {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        r.status,
        r.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
    let _ = stream.flush();
}

fn authorized(state: &State, req: &Request) -> bool {
    let Some(token) = &state.cfg.token else { return true };
    let want = format!("Bearer {token}");
    let got = req.auth.as_deref().unwrap_or("");
    got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

/// Returns the response and whether the server should stop afterwards.
fn route(state: &mut State, req: &Request) -> (Resp, bool) {
    if !authorized(state, req) {
        return (fail(401, "missing or invalid bearer token"), false);
    }
    let (m, p) = (req.method.as_str(), req.path.as_str());
    let resp = match (p, m) {
        ("/health", "GET") => ok(format!(
            "{{\"ok\":true,\"version\":\"{}\",\"cached\":{},\"allow_exec\":{},\"idle_timeout\":{}}}",
            env!("LMCLEAN_BUILD_VERSION"),
            state.cache.is_some(),
            state.cfg.allow_exec,
            state.cfg.idle.map_or(0, |d| d.as_secs())
        )
        .into_bytes()),
        ("/presets", "GET") => ok(state.presets_json()),
        ("/settings", "GET") => ok(state.settings_json()),
        ("/settings", "PUT" | "PATCH") => state.put_settings(&req.body),
        ("/scan", "POST") => state.scan(),
        ("/results", "GET") => match state.results_json(true) {
            Some(body) => ok(body),
            None => fail(404, "no cached results: POST /scan first"),
        },
        ("/clean", "POST") => state.clean(),
        ("/restore", "POST") => state.restore(&req.body),
        ("/shutdown", "POST") => return (ok(b"{\"ok\":true,\"shutdown\":true}".to_vec()), true),
        ("/health" | "/presets" | "/settings" | "/scan" | "/results" | "/clean" | "/restore" | "/shutdown", _) => {
            fail(405, "method not allowed")
        }
        _ => fail(404, "no such endpoint"),
    };
    (resp, false)
}

pub fn run(cfg: Config) -> i32 {
    let host = match host::Host::new() {
        Ok(h) => h,
        Err(e) => return report_error("run", &e),
    };
    let presets = match &cfg.presets {
        Some(file) => match load_presets(file) {
            Ok(p) => p,
            Err(e) => return report_app_failure(&e),
        },
        None => Presets { default: None, items: Vec::new() },
    };
    let start = match &cfg.preset {
        Some(name) => match presets.items.iter().any(|(n, _)| n == name) {
            true => Some(name.clone()),
            false => {
                println!("{}", error_json("cli", &Error::new("CLI_USAGE", format!("no such preset \"{name}\""))));
                return 2;
            }
        },
        None => presets.default.clone(),
    };
    let settings = start
        .as_ref()
        .and_then(|n| presets.items.iter().find(|(p, _)| p == n))
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let program = match &settings.program {
        Some(path) => match load_program(path, &host).and_then(|p| classify(&p).map(|_| p)) {
            Ok(p) => Some(p),
            Err(e) => return report_error("load", &Error::new("LOAD_FAILED", e)),
        },
        None => None,
    };
    // Resolve before binding so a non-loopback address without a token is refused.
    let addr = match cfg.listen.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            println!("{}", error_json("cli", &Error::new("CLI_USAGE", format!("cannot resolve --listen {}", cfg.listen))));
            return 2;
        }
    };
    if !addr.ip().is_loopback() && cfg.token.is_none() {
        println!("{}", error_json("cli", &Error::new("CLI_USAGE", "a non-loopback --listen address requires --token")));
        return 2;
    }
    let listener = match TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
        Ok(l) => l,
        Err(e) => return report_app_failure(&format!("cannot listen on {addr}: {e}")),
    };
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    let idle = cfg.idle;
    let mut state = State { cfg, presets, active: start, settings, program, host, cache: None };
    println!("{{\"listening\":\"{bound}\"}}");
    let _ = io::stdout().flush();

    let mut last = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let (resp, stop) = match read_request(&mut stream) {
                    Ok(req) => route(&mut state, &req),
                    Err(resp) => (resp, false),
                };
                write_response(&mut stream, &resp);
                last = Instant::now();
                if stop {
                    println!("{{\"shutdown\":\"requested\"}}");
                    return 0;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if idle.is_some_and(|limit| last.elapsed() >= limit) {
                    println!("{{\"shutdown\":\"idle\"}}");
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => eprintln!("lmclean: accept: {e}"),
        }
    }
}
