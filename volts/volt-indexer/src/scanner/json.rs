//! json: flattened key-path rows (`a.b[2].c`). Each top-level member (or
//! array element) is a block. Comments and trailing commas are tolerated so
//! jsonc files such as `tsconfig.json` or editor settings are indexed too.

use super::{clip, Block, BlockCtx, LineIndex, MAX_VALUE_CHARS};
use crate::index::IndexRow;

const MAX_DEPTH: usize = 128;

struct Parser<'a, 'r> {
    b: &'a [u8],
    pos: usize,
    end: usize,
    lines: &'a LineIndex,
    section: &'a str,
    rows: Option<&'r mut Vec<IndexRow>>,
}

fn join_key(path: &str, key: &str) -> String {
    let simple = !key.is_empty()
        && key.chars().all(|c| {
            c.is_alphanumeric() || c == '_' || c == '-' || c == '$' || c == '@'
        });
    match (path.is_empty(), simple) {
        (true, true) => key.to_string(),
        (false, true) => format!("{path}.{key}"),
        (_, false) => format!("{path}[{}]", serde_json::Value::from(key)),
    }
}

impl Parser<'_, '_> {
    fn skip_ws(&mut self) {
        while self.pos < self.end {
            match self.b[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'/' if self.b.get(self.pos + 1) == Some(&b'/') => {
                    while self.pos < self.end && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                b'/' if self.b.get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    while self.pos < self.end
                        && !self.b[self.pos..].starts_with(b"*/")
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.end);
                }
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        (self.pos < self.end).then(|| self.b[self.pos])
    }

    fn error<T>(&self, what: &str) -> Result<T, String> {
        Err(format!(
            "{what} at line {}",
            self.lines.line_of(self.pos) + 1
        ))
    }

    fn string(&mut self) -> Result<String, String> {
        // Caller has checked the opening quote.
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.end {
            let c = self.b[self.pos];
            self.pos += 1;
            match c {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let Some(e) = self.peek() else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let ch = self.unicode_escape()?;
                            let mut buf = [0; 4];
                            out.extend_from_slice(
                                ch.encode_utf8(&mut buf).as_bytes(),
                            );
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
        self.error("unterminated string")
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .b
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok());
        match digits {
            Some(v) => {
                self.pos += 4;
                Ok(v)
            }
            None => self.error("invalid \\u escape"),
        }
    }

    fn unicode_escape(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        if (0xD800..0xDC00).contains(&hi) && self.b[self.pos..].starts_with(b"\\u") {
            self.pos += 2;
            let lo = self.hex4()?;
            let code =
                0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
            return Ok(char::from_u32(code).unwrap_or('\u{FFFD}'));
        }
        Ok(char::from_u32(hi).unwrap_or('\u{FFFD}'))
    }

    fn emit(&mut self, key: &str, value: &str, start: usize) -> Option<usize> {
        let rows = self.rows.as_mut()?;
        let line = self.lines.line_of(start);
        rows.push(IndexRow::new(self.section, key, value, line, line));
        Some(rows.len() - 1)
    }

    fn finish(&mut self, row: Option<usize>, value: String, start: usize) {
        let end_line = self.lines.last_line(start, self.pos);
        if let (Some(rows), Some(row)) = (self.rows.as_mut(), row) {
            rows[row].value = value;
            rows[row].line_end = end_line;
        }
    }

    fn value(&mut self, path: &str, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return self.error("nesting too deep");
        }
        self.skip_ws();
        let start = self.pos;
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                let row = self.emit(path, "{}", start);
                let mut count = 0;
                loop {
                    self.skip_ws();
                    match self.peek() {
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        Some(b'"') => {}
                        _ => return self.error("expected key"),
                    }
                    let key = self.string()?;
                    self.skip_ws();
                    if self.peek() != Some(b':') {
                        return self.error("expected ':'");
                    }
                    self.pos += 1;
                    self.value(&join_key(path, &key), depth + 1)?;
                    count += 1;
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {}
                        _ => return self.error("expected ',' or '}'"),
                    }
                }
                self.finish(row, format!("{{{count} keys}}"), start);
            }
            Some(b'[') => {
                self.pos += 1;
                let row = self.emit(path, "[]", start);
                let mut count = 0;
                loop {
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        break;
                    }
                    self.value(&format!("{path}[{count}]"), depth + 1)?;
                    count += 1;
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {}
                        _ => return self.error("expected ',' or ']'"),
                    }
                }
                self.finish(row, format!("[{count} items]"), start);
            }
            Some(b'"') => {
                let s = self.string()?;
                let row = self.emit(path, "", start);
                self.finish(row, clip(&s, MAX_VALUE_CHARS), start);
            }
            Some(_) => {
                while self.pos < self.end
                    && !matches!(
                        self.b[self.pos],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' | b'/'
                    )
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return self.error("unexpected character");
                }
                let raw =
                    String::from_utf8_lossy(&self.b[start..self.pos]).into_owned();
                self.emit(path, &raw, start);
            }
            None => return self.error("unexpected end of input"),
        }
        Ok(())
    }
}

pub fn plan(text: &str, lines: &LineIndex) -> Vec<Block> {
    let whole = || {
        vec![Block {
            section: "$".to_string(),
            start: 0,
            end: text.len(),
            ctx: BlockCtx::Json {
                path: String::new(),
            },
        }]
    };
    let mut p = Parser {
        b: text.as_bytes(),
        pos: 0,
        end: text.len(),
        lines,
        section: "",
        rows: None,
    };
    p.skip_ws();
    let open = match p.peek() {
        Some(c @ (b'{' | b'[')) => c,
        Some(_) => return whole(),
        None => return Vec::new(),
    };
    p.pos += 1;
    let close = if open == b'{' { b'}' } else { b']' };
    let mut blocks = Vec::new();
    let mut index = 0;
    loop {
        p.skip_ws();
        if p.peek() == Some(close) {
            break;
        }
        let (section, path) = if open == b'{' {
            if p.peek() != Some(b'"') {
                return whole();
            }
            let Ok(key) = p.string() else { return whole() };
            p.skip_ws();
            if p.peek() != Some(b':') {
                return whole();
            }
            p.pos += 1;
            (key.clone(), join_key("", &key))
        } else {
            (format!("[{index}]"), format!("[{index}]"))
        };
        p.skip_ws();
        let start = p.pos;
        if p.value(&path, 1).is_err() {
            return whole();
        }
        blocks.push(Block {
            section,
            start,
            end: p.pos,
            ctx: BlockCtx::Json { path },
        });
        index += 1;
        p.skip_ws();
        match p.peek() {
            Some(b',') => p.pos += 1,
            Some(c) if c == close => {}
            _ => return whole(),
        }
    }
    blocks
}

pub fn analyze(text: &str, lines: &LineIndex, block: &Block) -> Vec<IndexRow> {
    let path = match &block.ctx {
        BlockCtx::Json { path } => path.as_str(),
        _ => "",
    };
    let mut rows = Vec::new();
    let mut p = Parser {
        b: text.as_bytes(),
        pos: block.start,
        end: block.end,
        lines,
        section: &block.section,
        rows: Some(&mut rows),
    };
    if let Err(err) = p.value(path, 0) {
        let line = lines.line_of(p.pos);
        rows.push(IndexRow::new(&block.section, "!error", &err, line, line));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"{
  // jsonc comment
  "name": "demo",
  "nested": { "a b": [1, {"c": true}], "u": "\u00e9\ud83d\ude00" },
  "list": [
    "x",
    null,
  ],
}"#;

    #[test]
    fn flattened_rows() {
        let lines = LineIndex::new(SRC);
        let blocks = plan(SRC, &lines);
        let sections: Vec<_> = blocks.iter().map(|b| b.section.as_str()).collect();
        assert_eq!(sections, vec!["name", "nested", "list"]);
        let rows: Vec<_> = blocks
            .iter()
            .flat_map(|b| analyze(SRC, &lines, b))
            .collect();
        let simple: Vec<_> = rows
            .iter()
            .map(|r| {
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
                ("name", "name", "demo", 2, 2),
                ("nested", "nested", "{2 keys}", 3, 3),
                ("nested", "nested[\"a b\"]", "[2 items]", 3, 3),
                ("nested", "nested[\"a b\"][0]", "1", 3, 3),
                ("nested", "nested[\"a b\"][1]", "{1 keys}", 3, 3),
                ("nested", "nested[\"a b\"][1].c", "true", 3, 3),
                ("nested", "nested.u", "é😀", 3, 3),
                ("list", "list", "[2 items]", 4, 7),
                ("list", "list[0]", "x", 5, 5),
                ("list", "list[1]", "null", 6, 6),
            ]
        );
    }

    #[test]
    fn root_array_and_errors() {
        let src = "[1, {\"a\": 2}]";
        let lines = LineIndex::new(src);
        let blocks = plan(src, &lines);
        assert_eq!(blocks.len(), 2);
        let rows = analyze(src, &lines, &blocks[1]);
        assert_eq!(rows[1].key, "[1].a");

        let bad = "{\"a\": [1, }";
        let lines = LineIndex::new(bad);
        let blocks = plan(bad, &lines);
        assert_eq!(blocks[0].section, "$");
        let rows = analyze(bad, &lines, &blocks[0]);
        assert_eq!(rows.last().unwrap().key, "!error");
    }
}
