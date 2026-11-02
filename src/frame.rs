//! Terminal return frame. lmcode host consumers (`run_lmcode`, the lmscript
//! driver) read exactly one envelope between sentinel lines as the last thing
//! on stdout; everything before it is passthrough output. Controlled failures
//! use the same frame.
use dslvm::{json_escape, Error};
use std::io::Write;

pub fn emit(envelope: &str) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "LMCODE_RETURN_BEGIN\n{envelope}\nLMCODE_RETURN_END\n");
    let _ = out.flush();
}

/// Success frame; `result_json` is JSON text. Returns the exit code (0).
pub fn ok(result_json: &str) -> i32 {
    emit(&format!("{{\"ok\":true,\"result\":{result_json}}}"));
    0
}

/// Failure frame for `phase` ("cli" | "load" | "run" | "app"). Returns the exit
/// code: 2 cli, 4 APP_FAIL, else 3.
pub fn error(phase: &str, e: &Error) -> i32 {
    // Compact ordered fields, as the frame consumer requires (dslvm's own
    // error_json puts spaces after the colons).
    let mut env = format!(
        "{{\"ok\":false,\"phase\":{},\"code\":{},\"message\":{}",
        json_escape(phase),
        json_escape(&e.code),
        json_escape(&e.message)
    );
    if let Some(line) = e.line {
        env.push_str(&format!(",\"line\":{line}"));
        if let Some(column) = e.column {
            env.push_str(&format!(",\"column\":{column}"));
        }
    }
    env.push('}');
    emit(&env);
    match (phase, e.code.as_str()) {
        ("cli", _) => 2,
        (_, "APP_FAIL") => 4,
        _ => 3,
    }
}

pub fn app_failure(message: &str) -> i32 {
    error("app", &Error::new("APP_FAIL", message))
}

pub fn usage(message: impl Into<String>) -> i32 {
    error("cli", &Error::new("CLI_USAGE", message))
}
