//! Row store and the queries answered from it.
//!
//! `workspace/symbol` maps rows to LSP `SymbolInformation` so they show up in
//! Lapce's workspace symbol palette (`#`). `indexer/query` returns raw rows for
//! tooling (agents, Ollama integrations, ...).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::scanner::{self, FileKind};

/// One indexed row. Line numbers are 0-based (LSP convention).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    pub section: String,
    pub row_no: usize,
    pub key: String,
    pub value: String,
    pub line_start: usize,
    pub line_end: usize,
}

impl IndexRow {
    pub fn new(
        section: &str,
        key: &str,
        value: &str,
        line_start: usize,
        line_end: usize,
    ) -> Self {
        Self {
            section: section.to_string(),
            row_no: 0,
            key: key.to_string(),
            value: value.to_string(),
            line_start,
            line_end: line_end.max(line_start),
        }
    }
}

// LSP SymbolKind values.
const MODULE: u32 = 2;
const CLASS: u32 = 5;
const METHOD: u32 = 6;
const PROPERTY: u32 = 7;
const FIELD: u32 = 8;
const ENUM: u32 = 10;
const INTERFACE: u32 = 11;
const FUNCTION: u32 = 12;
const VARIABLE: u32 = 13;
const CONSTANT: u32 = 14;
const ARRAY: u32 = 18;
const OBJECT: u32 = 19;
const KEY: u32 = 20;
const STRUCT: u32 = 23;
const TYPE_PARAMETER: u32 = 26;

/// Symbol kind for a row, or `None` for rows that are too fine-grained for
/// the symbol palette (attributes); those stay available via `indexer/query`.
fn symbol_kind(kind: FileKind, row: &IndexRow) -> Option<u32> {
    match kind {
        FileKind::Toml if row.value.starts_with('[') && row.key == row.section => {
            Some(MODULE)
        }
        FileKind::Toml => Some(PROPERTY),
        FileKind::Rust => {
            let in_impl =
                row.section.starts_with("impl ") || row.section.contains("> impl ");
            let in_trait = row
                .section
                .split(" > ")
                .last()
                .unwrap_or("")
                .starts_with("trait ");
            Some(match scanner::rust_item_keyword(row)? {
                "fn" if in_impl || in_trait => METHOD,
                "fn" | "macro_rules!" => FUNCTION,
                "struct" | "union" => STRUCT,
                "enum" => ENUM,
                "impl" => CLASS,
                "mod" => MODULE,
                "trait" => INTERFACE,
                "type" => TYPE_PARAMETER,
                "const" => CONSTANT,
                _ => VARIABLE,
            })
        }
        FileKind::Svg | FileKind::Xml | FileKind::Xaml => {
            (!row.section.ends_with("@attrs")).then_some(OBJECT)
        }
        FileKind::Json if row.key == "!error" => None,
        FileKind::Json
            if row.value.starts_with('{') && row.value.ends_with("keys}") =>
        {
            Some(OBJECT)
        }
        FileKind::Json
            if row.value.starts_with('[') && row.value.ends_with("items]") =>
        {
            Some(ARRAY)
        }
        FileKind::Json => Some(KEY),
        FileKind::Gguf => Some(match row.section.as_str() {
            "header" => CONSTANT,
            "tensors" => FIELD,
            _ => PROPERTY,
        }),
    }
}

/// Lower is better; `None` means no match.
fn match_score(name: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(3);
    }
    let name = name.to_lowercase();
    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else {
        let mut chars = name.chars();
        query.chars().all(|q| chars.any(|c| c == q)).then_some(3)
    }
}

pub fn path_to_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !raw.starts_with('/') {
        out.push('/');
    }
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut path = String::from_utf8(out).ok()?;
    // `/C:/x` -> `C:/x` for Windows hosts.
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        path.remove(0);
    }
    Some(PathBuf::from(path))
}

#[derive(Default)]
pub struct Index {
    files: HashMap<PathBuf, Vec<IndexRow>>,
}

impl Index {
    pub fn clear_file(&mut self, path: &Path) {
        self.files.remove(path);
    }

    /// Ensures an (empty) entry exists so indexed-but-empty files are counted.
    pub fn touch(&mut self, path: &Path) {
        self.files.entry(path.to_path_buf()).or_default();
    }

    /// Appends rows to a file, numbering them. Returns the file's row count.
    pub fn push_rows(&mut self, path: &Path, rows: Vec<IndexRow>) -> usize {
        let entry = self.files.entry(path.to_path_buf()).or_default();
        for mut row in rows {
            row.row_no = entry.len();
            entry.push(row);
        }
        entry.len()
    }

    pub fn rows(&self, path: &Path) -> Option<&[IndexRow]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn row_count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    fn sorted_files(&self) -> Vec<(&PathBuf, &Vec<IndexRow>)> {
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        files
    }

    /// `workspace/symbol` result: `SymbolInformation[]`.
    pub fn workspace_symbols(&self, query: &str, limit: usize) -> Value {
        let query = query.trim().to_lowercase();
        let mut hits = Vec::new();
        for (path, rows) in self.sorted_files() {
            let Some(kind) = path.to_str().and_then(FileKind::from_path) else {
                continue;
            };
            for row in rows {
                let Some(symbol_kind) = symbol_kind(kind, row) else {
                    continue;
                };
                if let Some(score) = match_score(&row.key, &query) {
                    hits.push((score, row.key.len(), path, row, symbol_kind));
                }
            }
        }
        hits.sort_by_key(|(score, len, ..)| (*score, *len));
        hits.truncate(limit);
        let symbols: Vec<Value> = hits
            .into_iter()
            .map(|(_, _, path, row, symbol_kind)| {
                let file_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let container = if row.section.is_empty() || row.section == row.key {
                    file_name
                } else {
                    format!("{file_name} › {}", row.section)
                };
                json!({
                    "name": row.key,
                    "kind": symbol_kind,
                    "location": {
                        "uri": path_to_uri(path),
                        "range": {
                            "start": { "line": row.line_start, "character": 0 },
                            "end": { "line": row.line_end, "character": 0 },
                        },
                    },
                    "containerName": container,
                })
            })
            .collect();
        Value::Array(symbols)
    }

    /// `indexer/query`: raw rows, filtered by the optional params
    /// `path` (path or file URI), `kind` (extension or list of extensions),
    /// `section` (exact match), `query` (case-insensitive substring of key or
    /// value), `offset` and `limit`.
    pub fn query(&self, params: &Value) -> Value {
        let str_param = |name: &str| params.get(name).and_then(Value::as_str);
        let path_filter = str_param("path")
            .map(|p| uri_to_path(p).unwrap_or_else(|| PathBuf::from(p)));
        let kinds: Option<Vec<FileKind>> = match params.get("kind") {
            Some(Value::String(k)) => {
                Some(FileKind::from_extension(k).into_iter().collect())
            }
            Some(Value::Array(ks)) => Some(
                ks.iter()
                    .filter_map(Value::as_str)
                    .filter_map(FileKind::from_extension)
                    .collect(),
            ),
            _ => None,
        };
        let section = str_param("section");
        let needle = str_param("query").map(str::to_lowercase);
        let offset =
            params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit =
            params.get("limit").and_then(Value::as_u64).unwrap_or(1000) as usize;

        let mut total = 0;
        let mut out = Vec::new();
        for (path, rows) in self.sorted_files() {
            if path_filter.as_ref().is_some_and(|p| p != path) {
                continue;
            }
            let kind = path.to_str().and_then(FileKind::from_path);
            if let (Some(kinds), Some(kind)) = (&kinds, kind) {
                if !kinds.contains(&kind) {
                    continue;
                }
            }
            for row in rows {
                if section.is_some_and(|s| s != row.section) {
                    continue;
                }
                if let Some(needle) = &needle {
                    if !row.key.to_lowercase().contains(needle)
                        && !row.value.to_lowercase().contains(needle)
                    {
                        continue;
                    }
                }
                total += 1;
                if total > offset && out.len() < limit {
                    let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
                    if let Value::Object(map) = &mut value {
                        map.insert("path".into(), json!(path.to_string_lossy()));
                        map.insert(
                            "kind".into(),
                            json!(kind.map(FileKind::extension)),
                        );
                    }
                    out.push(value);
                }
            }
        }
        json!({
            "total": total,
            "truncated": total > offset + out.len(),
            "files": self.file_count(),
            "rows": out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut index = Index::default();
        index.push_rows(
            Path::new("/ws/src/main.rs"),
            vec![
                IndexRow::new("", "main", "fn main()", 3, 9),
                IndexRow::new("", "#[derive]", "#[derive(Debug)]", 10, 10),
                IndexRow::new(
                    "impl Foo",
                    "frobnicate",
                    "pub fn frobnicate(&self)",
                    12,
                    14,
                ),
            ],
        );
        index.push_rows(
            Path::new("/ws/my dir/Cargo.toml"),
            vec![
                IndexRow::new("package", "package", "[package]", 0, 2),
                IndexRow::new("package", "name", "\"demo\"", 1, 1),
            ],
        );
        index
    }

    #[test]
    fn symbols() {
        let index = sample();
        let all = index.workspace_symbols("", 100);
        // Attribute rows are not symbols.
        assert_eq!(all.as_array().unwrap().len(), 4);

        let hits = index.workspace_symbols("frob", 10);
        assert_eq!(
            hits,
            json!([{
                "name": "frobnicate",
                "kind": METHOD,
                "location": {
                    "uri": "file:///ws/src/main.rs",
                    "range": {
                        "start": { "line": 12, "character": 0 },
                        "end": { "line": 14, "character": 0 },
                    },
                },
                "containerName": "main.rs › impl Foo",
            }])
        );
        let toml = index.workspace_symbols("pack", 10);
        assert_eq!(toml[0]["kind"], MODULE);
        assert_eq!(toml[0]["location"]["uri"], "file:///ws/my%20dir/Cargo.toml");
        // Exact matches sort first.
        let ranked = index.workspace_symbols("name", 10);
        assert_eq!(ranked[0]["name"], "name");
    }

    #[test]
    fn query_rows() {
        let index = sample();
        let res = index.query(&json!({ "kind": "rs", "query": "derive" }));
        assert_eq!(res["total"], 1);
        assert_eq!(res["rows"][0]["row_no"], 1);
        assert_eq!(res["rows"][0]["kind"], "rs");
        let res = index.query(&json!({ "path": "file:///ws/my%20dir/Cargo.toml" }));
        assert_eq!(res["total"], 2);
        let res = index.query(&json!({ "section": "package", "limit": 1 }));
        assert_eq!(res["rows"].as_array().unwrap().len(), 1);
        assert_eq!(res["truncated"], true);
    }

    #[test]
    fn uris() {
        assert_eq!(
            uri_to_path("file:///C:/a%20b/c.rs").unwrap(),
            PathBuf::from("C:/a b/c.rs")
        );
        assert_eq!(
            path_to_uri(Path::new("C:\\a b\\c.rs")),
            "file:///C:/a%20b/c.rs"
        );
        assert_eq!(
            uri_to_path("file:///home/x").unwrap(),
            PathBuf::from("/home/x")
        );
    }
}
