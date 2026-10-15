//! Minimal JSON-RPC transport for the Lapce wasi host.
//!
//! The host writes one JSON message per line to the plugin's stdin and then
//! calls the exported `handle_rpc`. The plugin answers by writing exactly one
//! message to stdout and calling `host_handle_rpc`, which makes the host parse
//! it (see `handle_plugin_server_message` in `lapce-proxy/src/plugin/psp.rs`).
//! Requests made by the plugin are answered synchronously: the response is on
//! stdin by the time `host_handle_rpc` returns.

use std::cell::Cell;
use std::io::{BufRead, Write};

use serde_json::{json, Value};

#[cfg(target_os = "wasi")]
#[link(wasm_import_module = "lapce")]
extern "C" {
    fn host_handle_rpc();
    fn host_handle_stderr();
}

#[cfg(not(target_os = "wasi"))]
unsafe fn host_handle_rpc() {}
#[cfg(not(target_os = "wasi"))]
unsafe fn host_handle_stderr() {}

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

pub fn read_message() -> Option<Value> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(value) => Some(value),
        Err(err) => {
            log(&format!("invalid message from host: {err}"));
            None
        }
    }
}

fn send(message: &Value) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{message}").and_then(|_| out.flush()).is_ok() {
        drop(out);
        unsafe { host_handle_rpc() };
    }
}

pub fn respond(id: Value, result: Value) {
    send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

pub fn respond_error(id: Value, code: i64, message: &str) {
    send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }));
}

pub fn notify(method: &str, params: Value) {
    send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

pub fn request(method: &str, params: Value) -> Result<Value, String> {
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    let response =
        read_message().ok_or_else(|| format!("no response to {method}"))?;
    if response.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(format!("unexpected response to {method}: {response}"));
    }
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("{method}: {message}"));
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

pub fn log(message: &str) {
    eprintln!("[volt-indexer] {message}");
    unsafe { host_handle_stderr() };
}

/// `host/executeProcess` from psp-types. Returns stdout on success.
pub fn execute_process(program: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let result = request(
        "host/executeProcess",
        json!({ "program": program, "args": args }),
    )?;
    let bytes = |field: &str| -> Vec<u8> {
        result
            .get(field)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    };
    if result.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(bytes("stdout"))
    } else {
        let stderr = bytes("stderr");
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ))
    }
}
