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
use crate::view::{self, Filter, Match, Page};
use crate::{trash, walk};
use crate::frame;
use dslvm::{Error, Program, Value};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SERVE_USAGE: &str = "usage: --serve [--lmcode FILE ...] [--listen ADDR:PORT] [--presets FILE] [--preset NAME] [--port-file FILE] [--idle-timeout SECS] [--allow-exec] [--token TOKEN]";
const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 16 << 10;

pub struct Config {
    port_file: Option<PathBuf>,
    programs: Vec<PathBuf>,
    listen: String,
    presets: Option<PathBuf>,
    preset: Option<String>,
    idle: Option<Duration>,
    allow_exec: bool,
    token: Option<String>,
}

pub fn parse_args(args: &[std::ffi::OsString]) -> Result<Config, String> {
    let mut cfg = Config {
        port_file: None,
        programs: Vec::new(),
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
            Some("--serve") => {}
            Some("--lmcode") => cfg.programs.push(PathBuf::from(value(&mut i, "--lmcode")?)),
            Some("--listen") => cfg.listen = value(&mut i, "--listen")?,
            Some("--presets") => cfg.presets = Some(PathBuf::from(value(&mut i, "--presets")?)),
            Some("--preset") => cfg.preset = Some(value(&mut i, "--preset")?),
            Some("--port-file") => cfg.port_file = Some(PathBuf::from(value(&mut i, "--port-file")?)),
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

/// Where a rule program comes from: a numbered `--lmcode` slot (what
/// `run_lmcode` supplies) or a bytecode file on disk.
#[derive(Clone, PartialEq)]
enum ProgramRef {
    Slot(usize),
    File(PathBuf),
}

#[derive(Clone, Default)]
struct Settings {
    program: Option<ProgramRef>,
    paths: Vec<PathBuf>,
    hidden: bool,
    max_depth: Option<u32>,
    trash: Option<PathBuf>,
    params: BTreeMap<String, String>,
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
            "program" => {
                s.program = Some(match v {
                    Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 1e6 => ProgramRef::Slot(*n as usize),
                    Json::Str(_) => ProgramRef::File(resolve(str_field(key, v)?)),
                    _ => return Err("\"program\" must be a slot number or a bytecode file path".into()),
                })
            }
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
            "params" => {
                // Merged into the current params; null removes a key.
                let Json::Obj(map) = v else {
                    return Err("\"params\" must be an object".into());
                };
                for (name, value) in map {
                    match value {
                        Json::Null => {
                            s.params.remove(name);
                        }
                        Json::Str(x) => {
                            s.params.insert(name.clone(), x.clone());
                        }
                        Json::Bool(b) => {
                            s.params.insert(name.clone(), b.to_string());
                        }
                        Json::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
                            s.params.insert(name.clone(), (*n as i64).to_string());
                        }
                        _ => return Err(format!("param \"{name}\" must be a string, integer, boolean or null")),
                    }
                }
            }
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

struct Cache {
    at: u64,
    matches: Vec<Match>,
}

struct State {
    cfg: Config,
    presets: Presets,
    active: Option<String>,
    settings: Settings,
    slots: Vec<Vec<u8>>,
    program: Option<Program>,
    host: host::Host,
    cache: Option<Cache>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_ref(r: &ProgramRef, slots: &[Vec<u8>], host: &host::Host) -> Result<Program, String> {
    match r {
        ProgramRef::Slot(i) => match slots.get(*i) {
            Some(bytes) => load_bytes(bytes, &format!("program slot {i}"), host),
            None => Err(format!("no program slot {i} ({} provided with --lmcode)", slots.len())),
        },
        ProgramRef::File(path) => {
            let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            load_bytes(&bytes, &path.display().to_string(), host)
        }
    }
}

fn load_bytes(bytes: &[u8], label: &str, host: &host::Host) -> Result<Program, String> {
    Program::load(bytes, crate::SCHEMA, &host.registry).map_err(|e| {
        let why = match e.code.as_str() {
            "LOAD_SCHEMA_MISMATCH" => "lmclean schema mismatch",
            "LOAD_UNBOUND" => "lmclean import mismatch",
            _ => "invalid lmclean bytecode",
        };
        format!("{label}: {why} ({})", e.code)
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
    *host.params.borrow_mut() = s.params.clone();
    program.run(&mut host.context, &host.registry).map_err(|e| e.message)?;
    let Roles { ignores, selects, reports } = classify(program)?;
    let roots: Vec<PathBuf> = if s.paths.is_empty() { vec![PathBuf::from(".")] } else { s.paths.clone() };
    let opts = walk::Options { hidden: s.hidden, max_depth: s.max_depth };
    let mut matches = Vec::new();
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
                let kind = if cand.metadata.is_dir() {
                    "dir"
                } else if cand.metadata.is_symlink() {
                    "symlink"
                } else {
                    "file"
                };
                let modified = cand
                    .metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                matches.push(Match { path: cand.path.clone(), root: root.clone(), size, kind, modified });
                Ok(false)
            })();
            let _ = host.context.take::<host::File>(obj);
            outcome
        })
        .map_err(|e| e.message)?;
    }
    Ok(Cache { at: now_secs(), matches })
}

// ---- JSON output helpers (byte-exact path escaping) ------------------------

fn jstr(out: &mut Vec<u8>, bytes: &[u8]) {
    let _ = write_json_string(out, bytes);
}

fn jparams(out: &mut Vec<u8>, params: &BTreeMap<String, String>) {
    out.push(b'{');
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        jstr(out, k.as_bytes());
        out.push(b':');
        jstr(out, v.as_bytes());
    }
    out.push(b'}');
}

fn jprogram(out: &mut Vec<u8>, p: &Option<ProgramRef>) {
    match p {
        Some(ProgramRef::Slot(i)) => out.extend_from_slice(i.to_string().as_bytes()),
        Some(ProgramRef::File(path)) => jstr(out, &walk::path_bytes(path)),
        None => out.extend_from_slice(b"null"),
    }
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
        jprogram(&mut o, &s.program);
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
        o.extend_from_slice(b",\"params\":");
        jparams(&mut o, &s.params);
        o.extend_from_slice(format!(",\"allow_exec\":{}}}", self.cfg.allow_exec).as_bytes());
        o
    }

    /// The cached scan seen through `filter`: totals, the selection's summary
    /// breakdowns and (unless summary-only) one page of matches.
    fn results_json(&self, cached: bool, filter: &Filter, page: &Page) -> Option<Vec<u8>> {
        let c = self.cache.as_ref()?;
        let now = now_secs() as i64;
        let sel = view::select(&c.matches, filter, now);
        let total_bytes: i64 = c.matches.iter().map(|m| m.size).sum();
        let mut o = format!(
            "{{\"ok\":true,\"cached\":{cached},\"scanned_at\":{},\"total_matched\":{},\"total_bytes\":{total_bytes},",
            c.at,
            c.matches.len()
        )
        .into_bytes();
        view::summary_fields(&sel, &mut o);
        if !page.summary_only {
            o.push(b',');
            view::detail_fields(&sel, page, now, &mut o);
        }
        o.push(b'}');
        Some(o)
    }

    /// Validates `next` completely, then commits it and drops the cache.
    fn commit(&mut self, next: Settings, active: Option<String>) -> Result<(), String> {
        if next.program != self.settings.program || self.program.is_none() {
            self.program = match &next.program {
                Some(p) => {
                    let program = load_ref(p, &self.slots, &self.host)?;
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
            jprogram(&mut o, &s.program);
            o.extend_from_slice(b",\"paths\":[");
            for (j, p) in s.paths.iter().enumerate() {
                if j > 0 {
                    o.push(b',');
                }
                jstr(&mut o, &walk::path_bytes(p));
            }
            o.extend_from_slice(format!("],\"hidden\":{},\"params\":", s.hidden).as_bytes());
            jparams(&mut o, &s.params);
            o.push(b'}');
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

    fn scan(&mut self, query: &str) -> Resp {
        let (filter, page) = match view::from_fields(&view::parse_query(query)) {
            Ok(v) => v,
            Err(e) => return fail(400, &e),
        };
        let Some(program) = &self.program else {
            return fail(409, "no program configured: choose a preset or PUT /settings with \"program\"");
        };
        match scan(program, &mut self.host, &self.settings) {
            Ok(cache) => {
                self.cache = Some(cache);
                ok(self.results_json(false, &filter, &page).unwrap_or_default())
            }
            Err(e) => fail(500, &e),
        }
    }

    fn results(&self, query: &str) -> Resp {
        let (filter, page) = match view::from_fields(&view::parse_query(query)) {
            Ok(v) => v,
            Err(e) => return fail(400, &e),
        };
        match self.results_json(true, &filter, &page) {
            Some(body) => ok(body),
            None => fail(404, "no cached results: POST /scan first"),
        }
    }

    /// Cleans the cached matches that pass `filter` (default: all of them).
    /// Body: `{"filter":{..},"dry_run":bool,"expect_matched":N,"expect_bytes":B}`.
    /// `expect_*` pin what the client reviewed: any drift is a 409 and nothing
    /// is touched. Unselected matches stay cached for a later round.
    fn clean(&mut self, body: &[u8]) -> Resp {
        let doc = if body.iter().all(u8::is_ascii_whitespace) {
            Json::Obj(vec![])
        } else {
            match std::str::from_utf8(body).map_err(|e| e.to_string()).and_then(json::parse) {
                Ok(d @ Json::Obj(_)) => d,
                Ok(_) => return fail(400, "body must be a JSON object"),
                Err(e) => return fail(400, &format!("invalid JSON: {e}")),
            }
        };
        let Json::Obj(items) = &doc else { unreachable!() };
        for (k, _) in items {
            if !["filter", "dry_run", "expect_matched", "expect_bytes"].contains(&k.as_str()) {
                return fail(400, &format!("unknown key \"{k}\""));
            }
        }
        let filter = match doc.get("filter") {
            None => Filter::default(),
            Some(f) => match Filter::from_json(f) {
                Ok(f) => f,
                Err(e) => return fail(400, &e),
            },
        };
        let dry_run = match doc.get("dry_run") {
            None => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => return fail(400, "\"dry_run\" must be a boolean"),
        };
        let expect = |key: &str| -> Result<Option<i64>, Resp> {
            match doc.get(key) {
                None => Ok(None),
                Some(Json::Num(n)) if n.fract() == 0.0 && *n >= 0.0 => Ok(Some(*n as i64)),
                Some(_) => Err(fail(400, &format!("\"{key}\" must be a nonnegative integer"))),
            }
        };
        let (want_matched, want_bytes) = match (expect("expect_matched"), expect("expect_bytes")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(r), _) | (_, Err(r)) => return r,
        };
        if !dry_run && !self.cfg.allow_exec {
            return fail(403, "deleting is disabled: start the server with --allow-exec (dry_run is always allowed)");
        }
        let Some(cache) = self.cache.as_mut() else {
            return fail(409, "no cached scan: POST /scan first");
        };
        let now = now_secs() as i64;
        let sel = view::select(&cache.matches, &filter, now);
        let sel_bytes: i64 = sel.iter().map(|m| m.size).sum();
        if want_matched.is_some_and(|n| n != sel.len() as i64) || want_bytes.is_some_and(|n| n != sel_bytes) {
            return Resp {
                status: 409,
                body: format!(
                    "{{\"ok\":false,\"error\":\"selection differs from expect_*\",\"matched\":{},\"bytes\":{sel_bytes}}}",
                    sel.len()
                )
                .into_bytes(),
            };
        }
        if dry_run {
            let mut o = b"{\"ok\":true,\"dry_run\":true,\"would_delete\":{".to_vec();
            view::summary_fields(&sel, &mut o);
            o.extend_from_slice(b"}}");
            return ok(o);
        }
        let mut sink = match &self.settings.trash {
            Some(dir) => match trash::Trash::open(dir) {
                Ok(t) => Some(t),
                Err(e) => return fail(500, &format!("trash: {e}")),
            },
            None => None,
        };
        let selected = sel.len();
        let mut done: Vec<PathBuf> = Vec::new(); // gone from disk, drop from cache
        let (mut deleted, mut missing, mut failed, mut bytes) = (0u64, 0u64, 0u64, 0u64);
        for m in &sel {
            let meta = match std::fs::symlink_metadata(&m.path) {
                Ok(meta) => meta,
                Err(_) => {
                    missing += 1;
                    done.push(m.path.clone());
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
                    done.push(m.path.clone());
                }
                Err(e) => {
                    eprintln!("lmclean: cannot remove {}: {e}", m.path.display());
                    failed += 1;
                }
            }
        }
        cache.matches.retain(|m| !done.contains(&m.path));
        let remaining = cache.matches.len();
        if remaining == 0 {
            self.cache = None;
        }
        let mode = if sink.is_some() { "trash" } else { "unlink" };
        let body = format!(
            "{{\"ok\":{},\"mode\":\"{mode}\",\"selected\":{selected},\"deleted\":{deleted},\"missing\":{missing},\"failed\":{failed},\"bytes\":{bytes},\"remaining\":{remaining}}}",
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
    query: String,
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
    let target = target.split('#').next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.trim_end_matches('/');
    Ok(Request {
        method: method.to_string(),
        path: if path.is_empty() { "/".into() } else { path.into() },
        query: query.to_string(),
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
        ("/scan", "POST") => state.scan(&req.query),
        ("/results", "GET") => state.results(&req.query),
        ("/clean", "POST") => state.clean(&req.body),
        ("/restore", "POST") => state.restore(&req.body),
        ("/shutdown", "POST") => return (ok(b"{\"ok\":true,\"shutdown\":true}".to_vec()), true),
        ("/health" | "/presets" | "/settings" | "/scan" | "/results" | "/clean" | "/restore" | "/shutdown", _) => {
            fail(405, "method not allowed")
        }
        _ => fail(404, "no such endpoint"),
    };
    (resp, false)
}

/// Terminal frame matching how we were invoked: one program is a single
/// envelope, several (a `lmcode_build` builder) a builder envelope with one
/// result per part, in add order.
fn exit_frame(parts: usize, how: &str) -> i32 {
    let part = format!("{{\"shutdown\":\"{how}\"}}");
    if parts < 2 {
        return frame::ok(&part);
    }
    let results = vec![part; parts].join(",");
    frame::emit(&format!("{{\"ok\":true,\"result\":[{results}],\"parts\":{parts}}}"));
    0
}

pub fn run(cfg: Config) -> i32 {
    let host = match host::Host::new() {
        Ok(h) => h,
        Err(e) => return frame::error("run", &e),
    };
    let presets = match &cfg.presets {
        Some(file) => match load_presets(file) {
            Ok(p) => p,
            Err(e) => return frame::app_failure(&e),
        },
        None => Presets { default: None, items: Vec::new() },
    };
    let start = match &cfg.preset {
        Some(name) => match presets.items.iter().any(|(n, _)| n == name) {
            true => Some(name.clone()),
            false => {
                return frame::usage(format!("no such preset \"{name}\""));
            }
        },
        None => presets.default.clone(),
    };
    let settings = start
        .as_ref()
        .and_then(|n| presets.items.iter().find(|(p, _)| p == n))
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let mut slots = Vec::new();
    for file in &cfg.programs {
        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(_) => return frame::app_failure("cannot read program input"),
        };
        if let Err(e) = load_bytes(&bytes, "--lmcode program", &host).and_then(|p| classify(&p).map(|_| ())) {
            return frame::error("load", &Error::new("LOAD_FAILED", e));
        }
        slots.push(bytes);
    }
    for (name, preset) in &presets.items {
        if let Some(ProgramRef::Slot(i)) = preset.program {
            if i >= slots.len() {
                return frame::app_failure(&format!(
                    "preset \"{name}\": program slot {i} but only {} --lmcode program(s) provided",
                    slots.len()
                ));
            }
        }
    }
    let mut settings = settings;
    // A lone program (what `run_lmcode` supplies for a single script) is the
    // implicit choice when no preset says otherwise.
    if settings.program.is_none() && slots.len() == 1 {
        settings.program = Some(ProgramRef::Slot(0));
    }
    let program = match &settings.program {
        Some(r) => match load_ref(r, &slots, &host).and_then(|p| classify(&p).map(|_| p)) {
            Ok(p) => Some(p),
            Err(e) => return frame::error("load", &Error::new("LOAD_FAILED", e)),
        },
        None => None,
    };
    // Resolve before binding so a non-loopback address without a token is refused.
    let addr = match cfg.listen.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            return frame::usage(format!("cannot resolve --listen {}", cfg.listen));
        }
    };
    if !addr.ip().is_loopback() && cfg.token.is_none() {
        return frame::usage("a non-loopback --listen address requires --token");
    }
    let listener = match TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
        Ok(l) => l,
        Err(e) => return frame::app_failure(&format!("cannot listen on {addr}: {e}")),
    };
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    let idle = cfg.idle;
    let mut state = State { cfg, presets, active: start, settings, slots, program, host, cache: None };
    // Written (atomically, via rename) once we accept connections: lmscript's
    // run_lmcode only relays host stdout when the host exits, so scripts find
    // the port here instead of on stdout.
    if let Some(file) = &state.cfg.port_file {
        let tmp = file.with_extension("tmp-lmclean");
        if let Err(e) = std::fs::write(&tmp, &bound).and_then(|()| std::fs::rename(&tmp, file)) {
            return frame::app_failure(&format!("cannot write --port-file: {e}"));
        }
    }
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
                    return exit_frame(state.slots.len(), "requested");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if idle.is_some_and(|limit| last.elapsed() >= limit) {
                    return exit_frame(state.slots.len(), "idle");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => eprintln!("lmclean: accept: {e}"),
        }
    }
}
