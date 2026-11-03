//! lmscript compiles the cleanup rules. This executable only loads verified
//! bytecode v6 and runs it against the files it walks. Dry-run is the default;
//! `--exec` deletes, `--trash DIR` moves matches into DIR with a manifest.
mod frame;
mod host;
mod json;
mod server;
mod trash;
mod view;
mod walk;

use dslvm::{Error, ObjRef, Program, Value};
use host::write_json_string;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

// Include exact bytes, including whitespace: the schema hash binds every program.
const SCHEMA: &[u8] = include_bytes!("../clean.dsl.json");
const USAGE: &str = "usage: --version | --serve [...] (see --serve usage) | --lmcode-schema [--json] | --lmcode-signatures | --lmcode FILE [--exec] [--trash DIR] [--restore] [--hidden] [--max-depth N] [--param NAME=VALUE ...] [PATH ...]";

fn main() {
    std::process::exit(match run() {
        Ok(code) => code,
        Err(_) => frame::app_failure("cannot read program input"),
    });
}

struct Cli {
    dsl: OsString,
    exec: bool,
    trash: Option<OsString>,
    restore: bool,
    hidden: bool,
    max_depth: Option<u32>,
    params: Vec<(String, String)>,
    paths: Vec<OsString>,
}

fn parse_cli(args: &[OsString]) -> Result<Cli, String> {
    let mut cli = Cli {
        dsl: args[1].clone(),
        exec: false,
        trash: None,
        restore: false,
        hidden: false,
        max_depth: None,
        params: Vec::new(),
        paths: Vec::new(),
    };
    let mut i = 2;
    while i < args.len() {
        let arg = &args[i];
        match arg.to_str() {
            Some("--exec") => cli.exec = true,
            Some("--restore") => cli.restore = true,
            Some("--hidden") => cli.hidden = true,
            Some("--trash") => {
                i += 1;
                let dir = args
                    .get(i)
                    .ok_or_else(|| "--trash needs a DIR".to_string())?;
                cli.trash = Some(dir.clone());
            }
            Some("--param") => {
                i += 1;
                let pair = args
                    .get(i)
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.split_once('='))
                    .ok_or_else(|| "--param needs NAME=VALUE".to_string())?;
                cli.params.push((pair.0.to_string(), pair.1.to_string()));
            }
            Some("--max-depth") => {
                i += 1;
                let value = args
                    .get(i)
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| "--max-depth needs a nonnegative integer N".to_string())?;
                cli.max_depth = Some(
                    value
                        .parse()
                        .map_err(|_| "--max-depth needs a nonnegative integer N".to_string())?,
                );
            }
            Some(flag) if flag.starts_with("--") => {
                return Err(format!("unknown flag {flag}"));
            }
            _ => cli.paths.push(arg.clone()),
        }
        i += 1;
    }
    if cli.restore && cli.exec {
        return Err("--restore cannot be combined with --exec".to_string());
    }
    if cli.restore && cli.trash.is_none() {
        return Err("--restore requires --trash DIR".to_string());
    }
    if !cli.restore && cli.trash.is_some() && !cli.exec {
        return Err("--trash requires --exec".to_string());
    }
    Ok(cli)
}

/// Per-run sweep state: the dispatch tables, the trash sink and the counters
/// behind the final summary line.
struct Sweep {
    ignores: Vec<usize>,
    selects: Vec<usize>,
    reports: Vec<usize>,
    trash: Option<trash::Trash>,
    matched: u64,
    bytes: u64,
    failed: u64,
}

impl Sweep {
    /// Runs the rule entries for one candidate and acts on a match. Returns
    /// whether the walker may descend: an ignored candidate is skipped
    /// entirely, and a matched directory is never re-filtered inside.
    fn visit(
        &mut self,
        program: &Program,
        host: &mut host::Host,
        cli: &Cli,
        cand: &walk::Candidate,
        obj: ObjRef,
        root: &Path,
    ) -> Result<bool, Error> {
        let arg = Value::object("File", obj);
        // Ignore rules prune first: an ignored candidate is not filtered, not
        // listed, and (when a directory) not descended into.
        for &index in &self.ignores {
            let value = program.call(index, &mut host.context, &host.registry, vec![arg.clone()])?;
            if value == Value::Bool(true) {
                return Ok(false);
            }
        }
        let mut is_match = false;
        for &index in &self.selects {
            let value = program.call(index, &mut host.context, &host.registry, vec![arg.clone()])?;
            if value == Value::Bool(true) {
                is_match = true;
            }
        }
        if !is_match {
            return Ok(true);
        }
        for &index in &self.reports {
            program.call(index, &mut host.context, &host.registry, vec![arg.clone()])?;
        }
        self.matched += 1;
        // Recursive size, computed once at match time only (never for
        // non-matches) and before any deletion: plans show reclaimable bytes.
        let size = walk::rsize(&cand.path);
        self.bytes += size as u64;
        let mut line = Vec::new();
        if !cli.exec {
            line.extend_from_slice(b"{\"plan\":\"delete\",\"path\":");
            write_json_string(&mut line, &walk::path_bytes(&cand.path))
                .map_err(|e| Error::host(format!("stdout: {e}")))?;
            line.extend_from_slice(format!(",\"size\":{size}}}").as_bytes());
            line.push(b'\n');
        } else if let Some(trash) = self.trash.as_mut() {
            match trash.store(&cand.path, root) {
                Ok(stored) => {
                    line.extend_from_slice(b"{\"deleted\":\"trash\",\"stored\":");
                    write_json_string(&mut line, &walk::path_bytes(&stored))
                        .map_err(|e| Error::host(format!("stdout: {e}")))?;
                    line.extend_from_slice(b",\"path\":");
                    write_json_string(&mut line, &walk::path_bytes(&cand.path))
                        .map_err(|e| Error::host(format!("stdout: {e}")))?;
                    line.extend_from_slice(format!(",\"size\":{size}}}").as_bytes());
                    line.push(b'\n');
                }
                Err(e) => {
                    eprintln!("lmclean: cannot trash {}: {e}", cand.path.display());
                    self.failed += 1;
                }
            }
        } else {
            let removal = walk::remove_entry(&cand.path, cand.metadata.is_dir());
            match removal {
                Ok(()) => {
                    line.extend_from_slice(b"{\"deleted\":\"unlink\",\"path\":");
                    write_json_string(&mut line, &walk::path_bytes(&cand.path))
                        .map_err(|e| Error::host(format!("stdout: {e}")))?;
                    line.extend_from_slice(format!(",\"size\":{size}}}").as_bytes());
                    line.push(b'\n');
                }
                Err(e) => {
                    eprintln!("lmclean: cannot delete {}: {e}", cand.path.display());
                    self.failed += 1;
                }
            }
        }
        if line.is_empty() {
            return Ok(false);
        }
        io::stdout()
            .lock()
            .write_all(&line)
            .map_err(|e| Error::host(format!("stdout: {e}")))?;
        Ok(false)
    }
}

fn file_of(cand: &walk::Candidate) -> host::File {
    let metadata = &cand.metadata;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    host::File {
        path: walk::path_bytes(&cand.path),
        name: cand.path.file_name().map(walk::os_bytes).unwrap_or_default(),
        extension: cand.path.extension().map(walk::os_bytes).unwrap_or_default(),
        size: metadata.len() as i64,
        depth: i64::from(cand.depth),
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.is_symlink(),
        modified,
    }
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut host = host::Host::new()?;
    if args.len() == 1 && args[0] == "--version" {
        println!("lmclean {}", env!("LMCLEAN_BUILD_VERSION"));
        return Ok(0);
    }
    if (args.len() == 1 || (args.len() == 2 && args[1] == "--json")) && args[0] == "--lmcode-schema" {
        io::stdout().lock().write_all(SCHEMA)?;
        return Ok(0);
    }
    if args.len() == 1 && args[0] == "--lmcode-signatures" {
        for signature in host.registry.signatures() {
            println!("{signature}");
        }
        return Ok(0);
    }
    if args.iter().any(|a| a == "--serve") {
        return Ok(match server::parse_args(&args) {
            Ok(cfg) => server::run(cfg),
            Err(message) => frame::usage(format!("{message}; {}", server::SERVE_USAGE)),
        });
    }
    if args.len() < 2 || args[0] != "--lmcode" {
        return Ok(frame::usage(USAGE));
    }
    let cli = match parse_cli(&args) {
        Ok(cli) => cli,
        Err(message) => {
            return Ok(frame::usage(format!("{message}; {USAGE}")));
        }
    };
    for (name, value) in &cli.params {
        host.params.borrow_mut().insert(name.clone(), value.clone());
    }
    let bytes = std::fs::read(&cli.dsl)?;
    let program = match Program::load(&bytes, SCHEMA, &host.registry) {
        Ok(program) => program,
        Err(error) => {
            // Normalize only load messages, exactly as the reference contract does.
            let message = match error.code.as_str() {
                "LOAD_SCHEMA_MISMATCH" => "lmclean schema mismatch",
                "LOAD_UNBOUND" => "lmclean import mismatch",
                _ => "invalid lmclean bytecode",
            };
            return Ok(frame::error("load", &Error::new(&error.code, message)));
        }
    };
    let mut out = io::stdout().lock();
    if cli.restore {
        // Restore replays the trash manifest; the program is loaded (one code
        // path) but never run, and nothing is walked.
        let dir = cli.trash.as_deref().map(PathBuf::from).unwrap_or_default();
        return match trash::restore(&dir, &mut out) {
            Ok(restored) => {
                out.flush()?;
                Ok(frame::ok(&format!("{{\"restored\":{restored}}}")))
            }
            Err(e) => Ok(frame::app_failure(&format!("restore: {e}"))),
        };
    }
    if let Err(error) = program.run(&mut host.context, &host.registry) {
        return Ok(frame::error("run", &error));
    }

    let mut sweep = Sweep {
        ignores: Vec::new(),
        selects: Vec::new(),
        reports: Vec::new(),
        trash: None,
        matched: 0,
        bytes: 0,
        failed: 0,
    };
    for (index, entry) in program.entries().iter().enumerate() {
        let unary_file = entry.params.as_slice() == ["File"];
        match entry.annotation.as_str() {
            "select" if unary_file && entry.returns == "bool" => sweep.selects.push(index),
            "select" => {
                return Ok(frame::app_failure(&format!(
                    "entry '{}' annotated @select must implement Filter(File) -> bool",
                    entry.name
                )));
            }
            "report" if unary_file && entry.returns == "void" => sweep.reports.push(index),
            "report" => {
                return Ok(frame::app_failure(&format!(
                    "entry '{}' annotated @report must implement Report(File) -> void",
                    entry.name
                )));
            }
            "ignore" if unary_file && entry.returns == "bool" => sweep.ignores.push(index),
            "ignore" => {
                return Ok(frame::app_failure(&format!(
                    "entry '{}' annotated @ignore must implement Ignore(File) -> bool",
                    entry.name
                )));
            }
            _ => {}
        }
    }
    if let (true, Some(dir)) = (&cli.exec, &cli.trash) {
        sweep.trash = match trash::Trash::open(Path::new(dir)) {
            Ok(trash) => Some(trash),
            Err(e) => return Ok(frame::app_failure(&format!("trash: {e}"))),
        };
    }
    let roots: Vec<OsString> = if cli.paths.is_empty() {
        vec![OsString::from(".")]
    } else {
        cli.paths.clone()
    };
    let opts = walk::Options {
        hidden: cli.hidden,
        max_depth: cli.max_depth,
    };
    for root in &roots {
        let root_path = PathBuf::from(root);
        let result = walk::walk(&root_path, &opts, &mut |cand| {
            let obj = host.context.insert("File", file_of(cand));
            let outcome = sweep.visit(&program, &mut host, &cli, cand, obj, &root_path);
            let _ = host.context.take::<host::File>(obj);
            outcome
        });
        if let Err(error) = result {
            return Ok(frame::error("run", &error));
        }
    }
    if sweep.failed > 0 {
        return Ok(frame::app_failure(&format!(
            "lmclean: {} deletion(s) failed ({} matched)",
            sweep.failed, sweep.matched
        )));
    }
    // The terminal return frame ({"ok": true, "result": ...}) that run_lmcode
    // and every other lmcode host consumer expects as the last stdout line.
    out.flush()?;
    Ok(frame::ok(&format!(
        "{{\"matched\":{},\"bytes\":{},\"dry_run\":{}}}",
        sweep.matched, sweep.bytes, !cli.exec
    )))
}
