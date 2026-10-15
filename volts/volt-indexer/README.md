# volt-indexer

A Lapce volt that builds a row-based index of the workspace's `.toml`, `.rs`,
`.svg`, `.gguf`, `.json`, `.xaml` and `.xml` files and serves it through
Lapce's workspace symbol search (`#` in the command palette).

## Build

```sh
cd volts/volt-indexer
cargo build --release --target wasm32-wasi
```

Rust 1.84+ renamed the target to `wasm32-wasip1`; on current toolchains use:

```sh
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

## Install

Copy the `.wasm` and `volt.toml` into a new folder in Lapce's plugins directory
and restart Lapce:

```sh
# Linux:   ~/.local/share/lapce-stable/plugins
# macOS:   ~/Library/Application Support/dev.lapce.Lapce-Stable/plugins
# Windows: %LOCALAPPDATA%\lapce\Lapce-Stable\data\plugins
PLUGINS=~/.local/share/lapce-stable/plugins
mkdir -p "$PLUGINS/drumnicorn.volt-indexer"
cp volt.toml target/wasm32-wasip1/release/volt-indexer.wasm "$PLUGINS/drumnicorn.volt-indexer/"
```

Use `lapce-nightly`/`Lapce-Nightly` (or `lapce-debug`/`Lapce-Debug` for a
local debug build of Lapce) instead of `lapce-stable`/`Lapce-Stable` for
other release channels.

The volt activates when the workspace contains any of the supported file
types (`[activation] workspace-contains` in `volt.toml`).

## How it works

- `src/main.rs`: PSP entry point. Handles `initialize`/`initialized`, re-indexes
  files on `textDocument/didSave`, answers `workspace/symbol` and
  `indexer/query`. The wasi sandbox only sees the volt's own folder, so files are
  listed and read on the host through `host/executeProcess`
  (`git ls-files`, falling back to `find`; ranged reads with `dd`; PowerShell
  on Windows).
- `src/scanner.rs` (+ `src/scanner/*`): extension dispatch. Each analyzer
  splits a file into blocks and emits
  `IndexRow { section, row_no, key, value, line_start, line_end }`
  (0-based lines):
  - toml: one block per `[section]`/`[[array]]`, one row per `key = value`.
  - rs: one block per top-level item; rows for `fn`, `struct`, `enum`,
    `impl`, `mod`, `trait`, `type`, `const`, `static`, `macro_rules!` and
    `#[...]` attributes (line-based lexer, no parser).
  - svg/xml/xaml: one row per element (`depth=N attrs=M`), attributes in a
    `<element path>@attrs` section.
  - json: flattened key-path rows (`a.b[0]["c d"]`), comments and trailing
    commas tolerated.
  - gguf: header, metadata KV section and tensor infos parsed with ranged
    reads; tensor data is never read.
- `src/queue.rs`: `VecDeque` of sub-tasks; every block (or batch of gguf
  entries) is a separate task, progress is reported with `$/progress`
  begin/report/end.
- `src/index.rs`: `HashMap<PathBuf, Vec<IndexRow>>`; maps rows to
  `SymbolInformation` for `workspace/symbol`.

## `indexer/query`

Custom request returning raw rows, for agents/Ollama integrations. All params
are optional:

```json
{ "path": "file:///ws/Cargo.toml", "kind": ["toml", "rs"], "section": "dependencies",
  "query": "serde", "offset": 0, "limit": 1000 }
```

Result:

```json
{ "root": "/ws", "files": 42, "total": 1, "truncated": false,
  "rows": [{ "path": "/ws/Cargo.toml", "kind": "toml", "section": "dependencies",
             "row_no": 3, "key": "serde", "value": "\"1\"", "line_start": 4, "line_end": 4 }] }
```

## Settings

`maxFileBytes` (default 2 MiB) skips larger text files; `maxSymbolResults`
(default 500) caps `workspace/symbol` results.

## Tests

```sh
cargo test
```
