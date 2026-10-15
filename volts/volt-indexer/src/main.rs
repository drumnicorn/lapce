//! Lapce volt that indexes workspace files into rows.
//!
//! The wasi sandbox only exposes the volt's own directory, so workspace files
//! are listed and read on the host through the PSP `host/executeProcess`
//! request (`git ls-files`/`find` and `dd`, or PowerShell on Windows).

mod index;
mod queue;
mod rpc;
mod scanner;

use std::cell::RefCell;
use std::path::Path;

use serde_json::{json, Value};

use index::{uri_to_path, Index};
use queue::{Host, TaskQueue};
use scanner::FileKind;

const METHOD_NOT_FOUND: i64 = -32601;
const BLOCK: u64 = 4096;

#[derive(Default)]
struct State {
    root: Option<String>,
    index: Index,
    queue: TaskQueue,
    max_symbols: usize,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        max_symbols: 500,
        ..Default::default()
    });
}

fn main() {}

/// Called by the host after it wrote one message to stdin.
#[no_mangle]
pub fn handle_rpc() {
    if let Some(message) = rpc::read_message() {
        STATE.with(|state| state.borrow_mut().handle(message));
    }
}

impl State {
    fn handle(&mut self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (message.get("id").cloned(), method) {
            (Some(id), Some(method)) => self.handle_request(id, method, params),
            (None, Some(method)) => self.handle_notification(method, params),
            _ => {}
        }
    }

    fn handle_request(&mut self, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => {
                self.initialize(&params);
                rpc::respond(
                    id,
                    json!({
                        "capabilities": {
                            "workspaceSymbolProvider": true,
                            "textDocumentSync": {
                                "openClose": false,
                                "change": 0,
                                "save": { "includeText": false },
                            },
                        },
                        "serverInfo": {
                            "name": "volt-indexer",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                );
            }
            "workspace/symbol" => {
                self.run_queue();
                let query =
                    params.get("query").and_then(Value::as_str).unwrap_or("");
                rpc::respond(
                    id,
                    self.index.workspace_symbols(query, self.max_symbols),
                );
            }
            "indexer/query" => {
                self.run_queue();
                let mut result = self.index.query(&params);
                result["root"] = json!(self.root);
                rpc::respond(id, result);
            }
            _ => rpc::respond_error(id, METHOD_NOT_FOUND, "method not supported"),
        }
    }

    fn handle_notification(&mut self, method: &str, params: Value) {
        match method {
            "initialized" => {
                if let Some(root) = self.root.clone() {
                    self.queue.enqueue_discover(&root);
                }
                self.run_queue();
            }
            "textDocument/didSave" => {
                let path = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(uri_to_path);
                if let Some(path) = path.as_deref().and_then(Path::to_str) {
                    if self.queue.enqueue_file(path) {
                        self.run_queue();
                    }
                }
            }
            _ => {}
        }
    }

    fn initialize(&mut self, params: &Value) {
        self.root = params
            .get("rootUri")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .pointer("/workspaceFolders/0/uri")
                    .and_then(Value::as_str)
            })
            .and_then(uri_to_path)
            .map(|p| {
                let root = p.to_string_lossy();
                match root.trim_end_matches(['/', '\\']) {
                    "" => root.into_owned(),
                    trimmed => trimmed.to_string(),
                }
            });
        let options = params.get("initializationOptions");
        let option =
            |name: &str| options.and_then(|o| o.get(name)).and_then(Value::as_u64);
        if let Some(max) = option("maxFileBytes") {
            self.queue.max_file_bytes = max;
        }
        if let Some(max) = option("maxSymbolResults") {
            self.max_symbols = max as usize;
        }
        rpc::log(&format!("workspace root: {:?}", self.root));
    }

    fn run_queue(&mut self) {
        let mut host = WasiHost;
        self.queue.run(&mut host, &mut self.index);
    }
}

struct WasiHost;

fn is_windows() -> bool {
    std::env::var("VOLT_OS").is_ok_and(|os| os == "windows")
}

fn powershell(script: String) -> Result<Vec<u8>, String> {
    rpc::execute_process(
        "powershell",
        &[
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ],
    )
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0 || *b == b'\n')
        .map(|s| {
            String::from_utf8_lossy(s)
                .trim_end_matches('\r')
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

impl Host for WasiHost {
    fn list_files(&mut self, root: &str) -> Result<Vec<String>, String> {
        let s = |v: &str| v.to_string();
        let git = rpc::execute_process(
            "git",
            &[
                s("-C"),
                s(root),
                s("ls-files"),
                s("-z"),
                s("--cached"),
                s("--others"),
                s("--exclude-standard"),
            ],
        );
        if let Ok(out) = git {
            let sep = if is_windows() { "\\" } else { "/" };
            return Ok(split_nul(&out)
                .into_iter()
                .filter(|f| FileKind::from_path(f).is_some())
                .map(|f| format!("{}{sep}{f}", root.trim_end_matches(['/', '\\'])))
                .collect());
        }

        let out = if is_windows() {
            let filter = FileKind::ALL
                .iter()
                .map(|k| format!("'.{}'", k.extension()))
                .collect::<Vec<_>>()
                .join(",");
            powershell(format!(
                "Get-ChildItem -LiteralPath {} -Recurse -File -ErrorAction SilentlyContinue \
                 | Where-Object {{ @({filter}) -contains $_.Extension.ToLower() \
                 -and $_.FullName -notmatch '[\\\\/](\\.git|target|node_modules)[\\\\/]' }} \
                 | ForEach-Object {{ $_.FullName }}",
                ps_quote(root)
            ))?
        } else {
            let mut args = vec![
                s(root),
                s("("),
                s("-name"),
                s(".git"),
                s("-o"),
                s("-name"),
                s("target"),
                s("-o"),
                s("-name"),
                s("node_modules"),
                s(")"),
                s("-prune"),
                s("-o"),
                s("-type"),
                s("f"),
                s("("),
            ];
            for (i, kind) in FileKind::ALL.iter().enumerate() {
                if i > 0 {
                    args.push(s("-o"));
                }
                args.push(s("-name"));
                args.push(format!("*.{}", kind.extension()));
            }
            args.extend([s(")"), s("-print0")]);
            rpc::execute_process("find", &args)?
        };
        Ok(split_nul(&out))
    }

    fn read_range(
        &mut self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if is_windows() {
            return powershell(format!(
                "$f=[IO.File]::OpenRead({}); try {{ [void]$f.Seek({offset},0); \
                 $b=New-Object byte[] {len}; $n=0; \
                 while($n -lt {len}){{ $r=$f.Read($b,$n,{len}-$n); if($r -le 0){{break}}; $n+=$r }}; \
                 $o=[Console]::OpenStandardOutput(); $o.Write($b,0,$n); $o.Flush() }} \
                 finally {{ $f.Close() }}",
                ps_quote(path)
            ));
        }
        let skip = offset / BLOCK;
        let head = (offset % BLOCK) as usize;
        let count = (head as u64 + len).div_ceil(BLOCK);
        let out = rpc::execute_process(
            "dd",
            &[
                format!("if={path}"),
                format!("bs={BLOCK}"),
                format!("skip={skip}"),
                format!("count={count}"),
            ],
        )?;
        let start = head.min(out.len());
        let end = (head + len as usize).min(out.len());
        Ok(out[start..end].to_vec())
    }

    fn progress(&mut self, token: &str, value: Value) {
        rpc::notify("$/progress", json!({ "token": token, "value": value }));
    }

    fn create_progress(&mut self, token: &str) {
        if let Err(err) =
            rpc::request("window/workDoneProgress/create", json!({ "token": token }))
        {
            rpc::log(&err);
        }
    }

    fn log(&mut self, message: &str) {
        rpc::log(message);
    }
}
