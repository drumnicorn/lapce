//! toml: every `[section]` / `[[array]]` header starts a block (a sub-task);
//! each `key = value` statement is a row. Multi-line arrays, inline tables
//! and strings are folded into a single row spanning several lines.

use std::collections::HashMap;

use super::{clip, lines_in, Block, BlockCtx, LineIndex, MAX_VALUE_CHARS};
use crate::index::IndexRow;

#[derive(Default, Clone, Copy)]
struct State {
    ml_basic: bool,
    ml_literal: bool,
    depth: u32,
}

impl State {
    fn in_value(&self) -> bool {
        self.ml_basic || self.ml_literal || self.depth > 0
    }
}

/// Scans one line, updating the multi-line state. Returns the length of the
/// line without its trailing comment.
fn scan_line(state: &mut State, line: &str, header: bool) -> usize {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if state.ml_basic {
            if b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i..].starts_with(b"\"\"\"") {
                state.ml_basic = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        if state.ml_literal {
            if b[i..].starts_with(b"'''") {
                state.ml_literal = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'#' => return i,
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                state.ml_basic = true;
                i += 3;
            }
            b'\'' if b[i..].starts_with(b"'''") => {
                state.ml_literal = true;
                i += 3;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'[' | b'{' if !header => {
                state.depth += 1;
                i += 1;
            }
            b']' | b'}' if !header => {
                state.depth = state.depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    b.len()
}

/// Position of the `=` separating a key from its value, ignoring quoted keys.
fn find_eq(code: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in code.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '=') => return Some(i),
            _ => {}
        }
    }
    None
}

fn header_name(code: &str) -> Option<(String, bool)> {
    let code = code.trim();
    if let Some(inner) = code.strip_prefix("[[") {
        let inner = inner.strip_suffix("]]").unwrap_or(inner);
        return Some((inner.trim().to_string(), true));
    }
    let inner = code.strip_prefix('[')?;
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    Some((inner.trim().to_string(), false))
}

pub fn plan(text: &str, lines: &LineIndex) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut state = State::default();
    let mut current = Block {
        section: String::new(),
        start: 0,
        end: 0,
        ctx: BlockCtx::None,
    };
    let mut has_content = false;
    let mut array_counts: HashMap<String, usize> = HashMap::new();

    for (_, offset, line) in lines_in(text, lines, 0, text.len()) {
        let header = !state.in_value() && line.trim_start().starts_with('[');
        let code_len = scan_line(&mut state, line, header);
        if header {
            if has_content || !current.section.is_empty() {
                current.end = offset;
                blocks.push(current.clone());
            }
            let (name, is_array) =
                header_name(&line[..code_len]).unwrap_or_default();
            let section = if is_array {
                let count = array_counts.entry(name.clone()).or_default();
                *count += 1;
                format!("{name}[{}]", *count - 1)
            } else {
                name
            };
            current = Block {
                section,
                start: offset,
                end: offset,
                ctx: BlockCtx::None,
            };
            has_content = true;
        } else if !line[..code_len].trim().is_empty() {
            has_content = true;
        }
    }
    if has_content {
        current.end = text.len();
        blocks.push(current);
    }
    blocks
}

pub fn analyze(text: &str, lines: &LineIndex, block: &Block) -> Vec<IndexRow> {
    let mut rows = Vec::new();
    let mut state = State::default();
    let mut pending: Option<(String, String, usize, usize)> = None;
    let mut last_content_line = lines.line_of(block.start);
    let mut header_row = None;

    for (line_no, _, line) in lines_in(text, lines, block.start, block.end) {
        let continuing = state.in_value();
        let header = !continuing && line.trim_start().starts_with('[');
        let code_len = scan_line(&mut state, line, header);
        let code = line[..code_len].trim();
        if !code.is_empty() {
            last_content_line = line_no;
        }

        if continuing {
            if let Some((_, value, _, end)) = pending.as_mut() {
                if !code.is_empty() {
                    value.push(' ');
                    value.push_str(code);
                }
                *end = line_no;
            }
        } else if header {
            header_row = Some(rows.len());
            rows.push(IndexRow::new(
                &block.section,
                &block.section,
                code,
                line_no,
                line_no,
            ));
        } else if let Some(eq) = find_eq(code) {
            pending = Some((
                code[..eq].trim().to_string(),
                code[eq + 1..].trim().to_string(),
                line_no,
                line_no,
            ));
        }

        if !state.in_value() {
            if let Some((key, value, start, end)) = pending.take() {
                rows.push(IndexRow::new(
                    &block.section,
                    &key,
                    &clip(&value, MAX_VALUE_CHARS),
                    start,
                    end,
                ));
            }
        }
    }
    if let Some((key, value, start, end)) = pending.take() {
        rows.push(IndexRow::new(
            &block.section,
            &key,
            &clip(&value, MAX_VALUE_CHARS),
            start,
            end,
        ));
    }
    if let Some(i) = header_row {
        rows[i].line_end = last_content_line;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"# top comment
name = "demo" # trailing
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive"] }
list = [
    "a", # inner
    "b[0]",
]

[[bin]]
name = "one"
text = """
[not a header]
"""

[[bin]]
"quoted.key" = 'x = y'
"#;

    fn rows(src: &str) -> Vec<(String, IndexRow)> {
        let lines = LineIndex::new(src);
        plan(src, &lines)
            .into_iter()
            .flat_map(|b| {
                analyze(src, &lines, &b)
                    .into_iter()
                    .map(move |r| (b.section.clone(), r))
            })
            .collect()
    }

    #[test]
    fn plans_sections() {
        let lines = LineIndex::new(SRC);
        let sections: Vec<_> =
            plan(SRC, &lines).into_iter().map(|b| b.section).collect();
        assert_eq!(sections, vec!["", "dependencies", "bin[0]", "bin[1]"]);
    }

    #[test]
    fn rows_per_key() {
        let rows = rows(SRC);
        let simple: Vec<_> = rows
            .iter()
            .map(|(_, r)| {
                (
                    r.section.as_str(),
                    r.key.as_str(),
                    r.value.as_str(),
                    r.line_start,
                    r.line_end,
                )
            })
            .collect();
        assert_eq!(
            simple,
            vec![
                ("", "name", "\"demo\"", 1, 1),
                ("", "version", "\"0.1.0\"", 2, 2),
                ("dependencies", "dependencies", "[dependencies]", 4, 9),
                (
                    "dependencies",
                    "serde",
                    "{ version = \"1\", features = [\"derive\"] }",
                    5,
                    5
                ),
                ("dependencies", "list", "[ \"a\", \"b[0]\", ]", 6, 9),
                ("bin[0]", "bin[0]", "[[bin]]", 11, 15),
                ("bin[0]", "name", "\"one\"", 12, 12),
                ("bin[0]", "text", "\"\"\" [not a header] \"\"\"", 13, 15),
                ("bin[1]", "bin[1]", "[[bin]]", 17, 18),
                ("bin[1]", "\"quoted.key\"", "'x = y'", 18, 18),
            ]
        );
    }
}
