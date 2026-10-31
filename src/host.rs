//! The File host type and the rule-facing functions (print, now).
//! Context owns every File handle; each handle lives for one walked candidate.
use dslvm::{Context, Error, ObjRef, Registry, Value};
use std::io::Write;

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
}

pub struct Host {
    pub registry: Registry,
    pub context: Context,
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
        register(&mut registry)?;
        Ok(Self {
            registry,
            context: Context::new(),
        })
    }
}

fn register(r: &mut Registry) -> Result<(), Error> {
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
    // print goes to stderr: stdout stays machine-clean for the JSON protocol.
    r.function("print", &["string"], "void", |_, a| {
        let mut err = std::io::stderr().lock();
        err.write_all(a[0].as_bytes()?)
            .map_err(|e| Error::host(format!("print: {e}")))?;
        err.write_all(b"\n")
            .map_err(|e| Error::host(format!("print: {e}")))?;
        Ok(Value::None)
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
