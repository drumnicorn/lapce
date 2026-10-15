//! rs: line-based item index. A tiny lexer tracks brace depth while skipping
//! strings, chars and comments; no Rust parser is involved. Top-level items
//! become blocks; rows are emitted for `fn`/`struct`/`enum`/`impl`/`mod`/
//! `trait`/`type`/`const`/`static`/`macro_rules!` items and `#[...]`
//! attributes at any depth.

use super::{clip, lines_in, Block, BlockCtx, LineIndex, MAX_VALUE_CHARS};
use crate::index::IndexRow;

#[derive(Clone, Copy, Default, PartialEq)]
enum Mode {
    #[default]
    Code,
    Str,
    RawStr(usize),
    BlockComment(u32),
}

#[derive(Clone, Copy, Debug, Default)]
struct LineInfo {
    /// The line starts in code (not inside a string or block comment).
    starts_in_code: bool,
    depth_start: i32,
    depth_end: i32,
    max_depth: i32,
    /// Smallest depth at which a `;` was seen.
    semi_depth: Option<i32>,
    last_code_char: Option<u8>,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn scan_lines(text: &str) -> Vec<LineInfo> {
    let mut mode = Mode::Code;
    let mut depth = 0i32;
    let mut infos = Vec::new();
    for line in text.split_inclusive('\n') {
        let b = line.as_bytes();
        let mut info = LineInfo {
            starts_in_code: mode == Mode::Code,
            depth_start: depth,
            max_depth: depth,
            ..Default::default()
        };
        let mut i = 0;
        while i < b.len() {
            match mode {
                Mode::Str => {
                    match b[i] {
                        b'\\' => i += 1,
                        b'"' => mode = Mode::Code,
                        _ => {}
                    }
                    i += 1;
                }
                Mode::RawStr(hashes) => {
                    if b[i] == b'"'
                        && b.len() >= i + 1 + hashes
                        && b[i + 1..i + 1 + hashes].iter().all(|c| *c == b'#')
                    {
                        mode = Mode::Code;
                        i += hashes;
                    }
                    i += 1;
                }
                Mode::BlockComment(n) => {
                    if b[i..].starts_with(b"*/") {
                        mode = if n == 1 {
                            Mode::Code
                        } else {
                            Mode::BlockComment(n - 1)
                        };
                        i += 2;
                    } else if b[i..].starts_with(b"/*") {
                        mode = Mode::BlockComment(n + 1);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::Code => {
                    let c = b[i];
                    if c.is_ascii_whitespace() {
                        i += 1;
                        continue;
                    }
                    if b[i..].starts_with(b"//") {
                        break;
                    }
                    if b[i..].starts_with(b"/*") {
                        mode = Mode::BlockComment(1);
                        i += 2;
                        continue;
                    }
                    info.last_code_char = Some(c);
                    match c {
                        b'"' => {
                            mode = Mode::Str;
                            i += 1;
                        }
                        b'r' if i == 0
                            || !is_ident(b[i - 1])
                            || b[i - 1] == b'b' =>
                        {
                            let mut j = i + 1;
                            while j < b.len() && b[j] == b'#' {
                                j += 1;
                            }
                            if j < b.len() && b[j] == b'"' {
                                mode = Mode::RawStr(j - i - 1);
                                i = j + 1;
                            } else {
                                i += 1;
                            }
                        }
                        b'\'' => {
                            if b.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                while i < b.len() && b[i] != b'\'' {
                                    i += 1;
                                }
                                i += 1;
                            } else if let Some(&next) = b.get(i + 1) {
                                let len = utf8_len(next);
                                if b.get(i + 1 + len) == Some(&b'\'') {
                                    i += len + 2;
                                } else {
                                    i += 1;
                                }
                            } else {
                                i += 1;
                            }
                        }
                        b'{' => {
                            depth += 1;
                            info.max_depth = info.max_depth.max(depth);
                            i += 1;
                        }
                        b'}' => {
                            depth -= 1;
                            i += 1;
                        }
                        b';' => {
                            info.semi_depth = Some(
                                info.semi_depth.map_or(depth, |d| d.min(depth)),
                            );
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
            }
        }
        info.depth_end = depth;
        infos.push(info);
    }
    infos
}

const QUALIFIERS: &[&str] = &["pub", "async", "unsafe", "extern", "default", "safe"];
const KEYWORDS: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "union",
    "impl",
    "mod",
    "trait",
    "type",
    "const",
    "static",
    "macro_rules!",
];

fn take_ident(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

/// Strips visibility and qualifiers, returning `(keyword, rest)`.
fn split_item(line: &str) -> Option<(&'static str, &str)> {
    let mut rest = line.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("pub(") {
            let close = r.find(')')?;
            rest = r[close + 1..].trim_start();
            continue;
        }
        if rest.starts_with('"') {
            // ABI string of `extern "C"`.
            let close = rest[1..].find('"')?;
            rest = rest[close + 2..].trim_start();
            continue;
        }
        let word = take_ident(rest);
        if QUALIFIERS.contains(&word) {
            rest = rest[word.len()..].trim_start();
            continue;
        }
        if word == "const" {
            let after = rest[5..].trim_start();
            let next = take_ident(after);
            if matches!(next, "fn" | "unsafe" | "async" | "extern") {
                rest = after;
                continue;
            }
        }
        break;
    }
    if let Some(after) = rest.strip_prefix("macro_rules!") {
        return Some(("macro_rules!", after.trim_start()));
    }
    let word = take_ident(rest);
    let keyword = KEYWORDS.iter().find(|k| **k == word)?;
    Some((keyword, rest[word.len()..].trim_start()))
}

/// The keyword of an item signature row (`value` holds the signature line).
pub fn item_keyword(signature: &str) -> Option<&'static str> {
    split_item(signature).map(|(k, _)| k)
}

fn skip_generics(s: &str) -> &str {
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return s[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    s
}

/// `(keyword, key)` for an item line, e.g. `("impl", "impl Display for Foo")`.
fn parse_item(line: &str) -> Option<(&'static str, String)> {
    let (keyword, rest) = split_item(line)?;
    if keyword == "impl" {
        let rest = skip_generics(rest);
        let end = [rest.find('{'), rest.find(" where")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        let target = rest[..end].split_whitespace().collect::<Vec<_>>().join(" ");
        if target.is_empty() {
            return None;
        }
        return Some((keyword, format!("impl {target}")));
    }
    let name = take_ident(rest);
    if name.is_empty() {
        return None;
    }
    Some((keyword, name.to_string()))
}

fn attribute_path(line: &str) -> Option<String> {
    let inner = line
        .strip_prefix("#![")
        .or_else(|| line.strip_prefix("#["))?
        .trim_start();
    let end = inner
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(inner.len());
    Some(inner[..end].to_string())
}

fn section_label(text: &str, lines: &LineIndex, start: usize, end: usize) -> String {
    for (_, _, line) in lines_in(text, lines, start, end) {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if t.starts_with("use ") || t.starts_with("pub use ") {
            return "use".to_string();
        }
        if let Some((keyword, key)) = parse_item(t) {
            return if keyword == "impl" {
                key
            } else {
                format!("{} {key}", keyword.trim_end_matches('!'))
            };
        }
        return clip(t, 40);
    }
    String::new()
}

pub fn plan(text: &str, lines: &LineIndex) -> Vec<Block> {
    let infos = scan_lines(text);
    let mut starts = Vec::new();
    let mut boundary = true;
    for ((_, offset, line), info) in lines_in(text, lines, 0, text.len()).zip(&infos)
    {
        let trimmed = line.trim();
        let is_doc = trimmed.starts_with("///") || trimmed.starts_with("//!");
        let is_comment = trimmed.starts_with("//") && !is_doc;
        if trimmed.is_empty() || is_comment || !info.starts_in_code {
            continue;
        }
        if boundary && info.depth_start == 0 {
            starts.push(offset);
        }
        boundary = if is_doc {
            false
        } else {
            info.depth_end == 0 && matches!(info.last_code_char, Some(b'}' | b';'))
        };
    }

    let mut blocks: Vec<Block> = Vec::new();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        let section = section_label(text, lines, start, end);
        match blocks.last_mut() {
            Some(prev) if section == "use" && prev.section == "use" => {
                prev.end = end
            }
            _ => blocks.push(Block {
                section,
                start,
                end,
                ctx: BlockCtx::None,
            }),
        }
    }
    blocks
}

pub fn analyze(text: &str, lines: &LineIndex, block: &Block) -> Vec<IndexRow> {
    let slice = &text[block.start..block.end];
    let infos = scan_lines(slice);
    let all: Vec<_> = lines_in(text, lines, block.start, block.end).collect();
    let mut rows = Vec::new();
    // (depth, label, last line index)
    let mut containers: Vec<(i32, String, usize)> = Vec::new();

    for (i, ((line_no, _, line), info)) in all.iter().zip(&infos).enumerate() {
        containers.retain(|(_, _, end)| *end >= i);
        if !info.starts_in_code {
            continue;
        }
        let trimmed = line.trim();
        let section = containers
            .iter()
            .map(|(_, label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(" > ");

        if let Some(path) = attribute_path(trimmed) {
            let mut value = trimmed.to_string();
            let mut balance = bracket_balance(trimmed);
            let mut end = i;
            while balance > 0 && end + 1 < all.len() {
                end += 1;
                let next = all[end].2.trim();
                balance += bracket_balance(next);
                value.push(' ');
                value.push_str(next);
            }
            rows.push(IndexRow::new(
                &section,
                &format!("#[{path}]"),
                &clip(&value, MAX_VALUE_CHARS),
                *line_no,
                all[end].0,
            ));
            continue;
        }

        let Some((keyword, key)) = parse_item(trimmed) else {
            continue;
        };
        let depth = info.depth_start;
        let mut opened = false;
        let mut end = all.len() - 1;
        for (j, next) in infos.iter().enumerate().skip(i) {
            opened |= next.max_depth > depth;
            if (opened && next.depth_end <= depth)
                || (!opened && next.semi_depth == Some(depth))
            {
                end = j;
                break;
            }
        }
        let signature = trimmed.trim_end_matches('{').trim_end();
        rows.push(IndexRow::new(
            &section,
            &key,
            &clip(signature, MAX_VALUE_CHARS),
            *line_no,
            all[end].0,
        ));
        if opened && matches!(keyword, "impl" | "mod" | "trait") {
            let label = if keyword == "impl" {
                key
            } else {
                format!("{keyword} {key}")
            };
            containers.push((depth, label, end));
        }
    }
    rows
}

fn bracket_balance(s: &str) -> i32 {
    s.bytes().fold(0, |acc, b| match b {
        b'[' => acc + 1,
        b']' => acc - 1,
        _ => acc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r##"use std::fmt;
use std::{
    io,
    path::Path,
};

/// Docs for Foo.
#[derive(Debug, Clone)]
pub struct Foo {
    s: &'static str,
}

impl<T: fmt::Debug> fmt::Display for Wrapper<T>
where
    T: Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let brace = '{';
        let s = "fn fake() {";
        let r = r#"}"#;
        write!(f, "{brace}{s}{r}")
    }
}

pub(crate) mod inner {
    #[cfg(test)]
    pub const fn answer() -> u8 { 42 }
    pub trait Named {
        fn name(&self) -> &str;
    }
}

pub enum Kind { A, B }
macro_rules! noop { () => {}; }
"##;

    fn run(src: &str) -> (Vec<Block>, Vec<IndexRow>) {
        let lines = LineIndex::new(src);
        let blocks = plan(src, &lines);
        let rows = blocks
            .iter()
            .flat_map(|b| analyze(src, &lines, b))
            .collect();
        (blocks, rows)
    }

    #[test]
    fn plans_top_level_items() {
        let (blocks, _) = run(SRC);
        let sections: Vec<_> = blocks.iter().map(|b| b.section.as_str()).collect();
        assert_eq!(
            sections,
            vec![
                "use",
                "struct Foo",
                "impl fmt::Display for Wrapper<T>",
                "mod inner",
                "enum Kind",
                "macro_rules noop",
            ]
        );
    }

    #[test]
    fn rows_for_items_and_attributes() {
        let (_, rows) = run(SRC);
        let simple: Vec<_> = rows
            .iter()
            .map(|r| (r.section.as_str(), r.key.as_str(), r.line_start, r.line_end))
            .collect();
        assert_eq!(
            simple,
            vec![
                ("", "#[derive]", 7, 7),
                ("", "Foo", 8, 10),
                ("", "impl fmt::Display for Wrapper<T>", 12, 22),
                ("impl fmt::Display for Wrapper<T>", "fmt", 16, 21),
                ("", "inner", 24, 30),
                ("mod inner", "#[cfg]", 25, 25),
                ("mod inner", "answer", 26, 26),
                ("mod inner", "Named", 27, 29),
                ("mod inner > trait Named", "name", 28, 28),
                ("", "Kind", 32, 32),
                ("", "noop", 33, 33),
            ]
        );
        assert_eq!(rows[1].value, "pub struct Foo");
        assert_eq!(item_keyword(&rows[6].value), Some("fn"));
        assert_eq!(item_keyword(&rows[2].value), Some("impl"));
    }
}
