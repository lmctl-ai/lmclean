//! The File host type and the rule-facing functions (print, now).
//! Context owns every File handle; each handle lives for one walked candidate.
use dslvm::{Context, Error, ObjRef, Registry, Value};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

/// Client-tunable rule parameters, read by rules through `param_int` /
/// `param_str`. Shared with the registry closures; the server swaps the
/// contents before each scan, the CLI fills it from `--param NAME=VALUE`.
pub type Params = Rc<RefCell<BTreeMap<String, String>>>;

/// One walked filesystem candidate. Strings are raw bytes: Unix paths are not UTF-8.
pub struct File {
    pub path: Vec<u8>,
    pub name: Vec<u8>,
    pub extension: Vec<u8>,
    pub size: i64,
    pub depth: i64,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub modified: i64,
    pub accessed: i64,
    pub rtimes: Cell<Option<(i64, i64)>>,
    pub git: OnceCell<crate::git::GitInfo>,
}

impl File {
    fn rtimes(&self) -> (i64, i64) {
        if let Some(t) = self.rtimes.get() {
            return t;
        }
        let t = crate::walk::rtimes(&crate::walk::path_from_bytes(&self.path));
        self.rtimes.set(Some(t));
        t
    }

    fn git_info(&self) -> crate::git::GitInfo {
        *self.git.get_or_init(|| {
            let _ = self.rtimes();
            crate::git::inspect(&crate::walk::path_from_bytes(&self.path))
        })
    }
}

pub struct Host {
    pub registry: Registry,
    pub context: Context,
    pub params: Params,
}

fn file_ref(ctx: &Context, obj: ObjRef) -> Result<&File, Error> {
    ctx.get::<File>(obj)
        .map_err(|_| Error::host("file: invalid handle"))
}

impl Host {
    pub fn new() -> Result<Self, Error> {
        // Default std registers strings, math, containers, JSON and files: the
        // implicit lmcode std catalog that compiled rules may import.
        let mut registry = Registry::new();
        let params = Params::default();
        register(&mut registry, params.clone())?;
        Ok(Self {
            registry,
            context: Context::new(),
            params,
        })
    }
}

fn register(r: &mut Registry, params: Params) -> Result<(), Error> {
    r.method("File", "path", &[], "string", |ctx, this, _| {
        Ok(Value::string(file_ref(ctx, this)?.path.as_slice()))
    })?;
    r.method("File", "name", &[], "string", |ctx, this, _| {
        Ok(Value::string(file_ref(ctx, this)?.name.as_slice()))
    })?;
    r.method("File", "extension", &[], "string", |ctx, this, _| {
        Ok(Value::string(file_ref(ctx, this)?.extension.as_slice()))
    })?;
    r.method("File", "size", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.size))
    })?;
    r.method("File", "rsize", &[], "int", |ctx, this, _| {
        let file = file_ref(ctx, this)?;
        Ok(Value::Int(crate::walk::rsize(&crate::walk::path_from_bytes(
            &file.path,
        ))))
    })?;
    r.method("File", "depth", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.depth))
    })?;
    r.method("File", "is_file", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.is_file))
    })?;
    r.method("File", "is_dir", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.is_dir))
    })?;
    r.method("File", "is_symlink", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.is_symlink))
    })?;
    r.method("File", "modified", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.modified))
    })?;
    r.method("File", "accessed", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.accessed))
    })?;
    r.method("File", "raccessed", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.rtimes().0))
    })?;
    r.method("File", "rmodified", &[], "int", |ctx, this, _| {
        Ok(Value::Int(file_ref(ctx, this)?.rtimes().1))
    })?;
    r.method("File", "is_git_repo", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.git_info().is_repo))
    })?;
    r.method("File", "is_git_clean", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.git_info().is_clean))
    })?;
    r.method("File", "is_git_pushed", &[], "bool", |ctx, this, _| {
        Ok(Value::Bool(file_ref(ctx, this)?.git_info().is_pushed))
    })?;
    r.method("File", "git_status", &[], "string", |ctx, this, _| {
        Ok(Value::string(file_ref(ctx, this)?.git_info().status.as_bytes()))
    })?;
    // print goes to stderr: stdout stays machine-clean for the JSON protocol.
    r.function("print", &["string"], "void", |_, a| {
        let mut err = std::io::stderr().lock();
        err.write_all(a[0].as_bytes()?)
            .map_err(|e| Error::host(format!("print: {e}")))?;
        err.write_all(b"\n")
            .map_err(|e| Error::host(format!("print: {e}")))?;
        Ok(Value::None)
    })?;
    let p = params.clone();
    r.function("param_int", &["string", "int"], "int", move |_, a| {
        let name = a[0].to_string_lossy()?;
        match p.borrow().get(&name) {
            None => Ok(a[1].clone()),
            Some(v) => v
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| Error::host(format!("param {name}: {v:?} is not an integer"))),
        }
    })?;
    r.function("param_str", &["string", "string"], "string", move |_, a| {
        let name = a[0].to_string_lossy()?;
        match params.borrow().get(&name) {
            None => Ok(a[1].clone()),
            Some(v) => Ok(Value::string(v.as_bytes())),
        }
    })?;
    r.function("now", &[], "int", |_, _| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::host(format!("now: {e}")))?;
        Ok(Value::Int(secs.as_secs() as i64))
    })?;
    Ok(())
}

/// Byte-exact JSON string literal writer (paths are byte strings, not text).
pub fn write_json_string(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for &byte in bytes {
        match byte {
            b'"' => out.write_all(b"\\\"")?,
            b'\\' => out.write_all(b"\\\\")?,
            b'\n' => out.write_all(b"\\n")?,
            b'\r' => out.write_all(b"\\r")?,
            b'\t' => out.write_all(b"\\t")?,
            0x00..=0x1f => write!(out, "\\u{byte:04x}")?,
            _ => out.write_all(&[byte])?,
        }
    }
    out.write_all(b"\"")
}
