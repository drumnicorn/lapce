//! svg/xml/xaml: one row per element (`depth=N` in the value, the element's
//! path as its section) followed by one row per attribute in an
//! `<path>@attrs` section. Each direct child of the root element is a block.

use std::collections::HashMap;

use super::{clip, Block, BlockCtx, LineIndex, MAX_VALUE_CHARS};
use crate::index::IndexRow;

const ID_ATTRS: &[&str] = &["id", "x:Name", "Name", "x:Key", "name"];

#[derive(Debug)]
enum Tok<'a> {
    Start {
        name: &'a str,
        attrs: Vec<(&'a str, &'a str, usize)>,
        self_closing: bool,
        start: usize,
        end: usize,
    },
    End {
        name: &'a str,
        end: usize,
    },
}

struct Tokens<'a> {
    s: &'a str,
    pos: usize,
    end: usize,
}

impl<'a> Tokens<'a> {
    fn new(s: &'a str, start: usize, end: usize) -> Self {
        Self { s, pos: start, end }
    }

    fn find_from(&self, from: usize, pat: &str) -> usize {
        self.s[from..self.end]
            .find(pat)
            .map_or(self.end, |i| from + i + pat.len())
    }
}

fn is_name_char(b: u8) -> bool {
    !(b.is_ascii_whitespace() || matches!(b, b'/' | b'>' | b'=' | b'<'))
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Tok<'a>;

    fn next(&mut self) -> Option<Tok<'a>> {
        let b = self.s.as_bytes();
        loop {
            let lt = self.pos + self.s[self.pos..self.end].find('<')?;
            let rest = &self.s[lt..self.end];
            if rest.starts_with("<!--") {
                self.pos = self.find_from(lt + 4, "-->");
            } else if rest.starts_with("<![CDATA[") {
                self.pos = self.find_from(lt + 9, "]]>");
            } else if rest.starts_with("<?") {
                self.pos = self.find_from(lt + 2, "?>");
            } else if rest.starts_with("<!") {
                let mut depth = 0;
                let mut i = lt + 2;
                while i < self.end {
                    match b[i] {
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        b'>' if depth <= 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
                self.pos = (i + 1).min(self.end);
            } else if let Some(close) = rest.strip_prefix("</") {
                let len = close.find('>').unwrap_or(close.len());
                self.pos = (lt + 2 + len + 1).min(self.end);
                return Some(Tok::End {
                    name: close[..len].trim(),
                    end: self.pos,
                });
            } else if rest.len() > 1 && is_name_char(b[lt + 1]) {
                return Some(self.start_tag(lt));
            } else {
                self.pos = lt + 1;
            }
        }
    }
}

impl<'a> Tokens<'a> {
    fn start_tag(&mut self, lt: usize) -> Tok<'a> {
        let b = self.s.as_bytes();
        let mut i = lt + 1;
        while i < self.end && is_name_char(b[i]) {
            i += 1;
        }
        let name = &self.s[lt + 1..i];
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            while i < self.end && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= self.end {
                break;
            }
            if b[i] == b'>' {
                i += 1;
                break;
            }
            if b[i..self.end].starts_with(b"/>") {
                self_closing = true;
                i += 2;
                break;
            }
            let attr_start = i;
            while i < self.end && is_name_char(b[i]) {
                i += 1;
            }
            if i == attr_start {
                // Stray character such as a lone `/` or `<`.
                i += 1;
                continue;
            }
            let attr_name = &self.s[attr_start..i];
            while i < self.end && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let mut value = "";
            if i < self.end && b[i] == b'=' {
                i += 1;
                while i < self.end && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < self.end && (b[i] == b'"' || b[i] == b'\'') {
                    let quote = b[i] as char;
                    let close = self.s[i + 1..self.end]
                        .find(quote)
                        .map_or(self.end, |c| i + 1 + c);
                    value = &self.s[i + 1..close];
                    i = (close + 1).min(self.end);
                } else {
                    let v_start = i;
                    while i < self.end && !b[i].is_ascii_whitespace() && b[i] != b'>'
                    {
                        i += 1;
                    }
                    value = &self.s[v_start..i];
                }
            }
            attrs.push((attr_name, value, attr_start));
        }
        self.pos = i;
        Tok::Start {
            name,
            attrs,
            self_closing,
            start: lt,
            end: i,
        }
    }
}

fn segment(name: &str, depth: usize, index: usize) -> String {
    if depth == 0 {
        name.to_string()
    } else {
        format!("{name}[{index}]")
    }
}

pub fn plan(text: &str, lines: &LineIndex) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut root: Option<String> = None;
    let mut depth = 0usize;
    let mut child_start = 0;
    let mut child_index = 0;
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut root_close = None;

    for tok in Tokens::new(text, 0, text.len()) {
        match tok {
            Tok::Start {
                name,
                self_closing,
                start,
                end,
                ..
            } => {
                if depth == 0 {
                    if root.is_some() {
                        continue;
                    }
                    root = Some(name.to_string());
                    blocks.push(Block {
                        section: name.to_string(),
                        start: 0,
                        end,
                        ctx: BlockCtx::Markup {
                            depth: 0,
                            parent: Vec::new(),
                            index: 0,
                            unclosed_end_line: None,
                        },
                    });
                    if self_closing {
                        break;
                    }
                    depth = 1;
                    continue;
                }
                if depth == 1 {
                    child_start = start;
                    let count = counts.entry(name.to_string()).or_default();
                    child_index = *count;
                    *count += 1;
                }
                if self_closing {
                    if depth == 1 {
                        push_child(
                            &mut blocks,
                            &root,
                            name,
                            child_index,
                            start,
                            end,
                        );
                    }
                } else {
                    depth += 1;
                }
            }
            Tok::End { name, end } => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 1 {
                    let child = &text[child_start..end];
                    let child_name = child[1..]
                        .split(|c: char| !is_name_char(c as u8))
                        .next()
                        .unwrap_or(name);
                    push_child(
                        &mut blocks,
                        &root,
                        child_name,
                        child_index,
                        child_start,
                        end,
                    );
                } else if depth == 0 {
                    root_close = Some(end);
                    break;
                }
            }
        }
    }
    if depth > 1 {
        let child = &text[child_start..];
        let child_name = child[1..]
            .split(|c: char| !is_name_char(c as u8))
            .next()
            .unwrap_or("");
        push_child(
            &mut blocks,
            &root,
            child_name,
            child_index,
            child_start,
            text.len(),
        );
    }
    if let Some(Block {
        ctx: BlockCtx::Markup {
            unclosed_end_line, ..
        },
        ..
    }) = blocks.first_mut()
    {
        *unclosed_end_line =
            Some(lines.last_line(0, root_close.unwrap_or(text.len())));
    }
    blocks
}

fn push_child(
    blocks: &mut Vec<Block>,
    root: &Option<String>,
    name: &str,
    index: usize,
    start: usize,
    end: usize,
) {
    let root = root.clone().unwrap_or_default();
    blocks.push(Block {
        section: format!("{root}/{}", segment(name, 1, index)),
        start,
        end,
        ctx: BlockCtx::Markup {
            depth: 1,
            parent: vec![root],
            index,
            unclosed_end_line: None,
        },
    });
}

struct Open {
    name: String,
    segment: String,
    row: usize,
    counts: HashMap<String, usize>,
}

pub fn analyze(text: &str, lines: &LineIndex, block: &Block) -> Vec<IndexRow> {
    let (base_depth, parent, first_index, unclosed_end_line) = match &block.ctx {
        BlockCtx::Markup {
            depth,
            parent,
            index,
            unclosed_end_line,
        } => (*depth, parent.clone(), *index, *unclosed_end_line),
        _ => (0, Vec::new(), 0, None),
    };
    let mut rows = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut top_level_seen = false;

    for tok in Tokens::new(text, block.start, block.end) {
        match tok {
            Tok::Start {
                name,
                attrs,
                self_closing,
                start,
                end,
            } => {
                let depth = base_depth + stack.len();
                let index = match stack.last_mut() {
                    Some(open) => {
                        let count = open.counts.entry(name.to_string()).or_default();
                        *count += 1;
                        *count - 1
                    }
                    None if !top_level_seen => {
                        top_level_seen = true;
                        first_index
                    }
                    None => first_index + 1,
                };
                let seg = segment(name, depth, index);
                let path = parent
                    .iter()
                    .map(String::as_str)
                    .chain(stack.iter().map(|o| o.segment.as_str()))
                    .chain(std::iter::once(seg.as_str()))
                    .collect::<Vec<_>>()
                    .join("/");
                let id = attrs
                    .iter()
                    .find(|(n, _, _)| ID_ATTRS.contains(n))
                    .map(|(_, v, _)| *v);
                let key = match id {
                    Some(id) if !id.is_empty() => format!("{name}#{id}"),
                    _ => name.to_string(),
                };
                let line_start = lines.line_of(start);
                let row = rows.len();
                rows.push(IndexRow::new(
                    &path,
                    &key,
                    &format!("depth={depth} attrs={}", attrs.len()),
                    line_start,
                    lines.last_line(start, end),
                ));
                let attr_section = format!("{path}@attrs");
                for (attr, value, offset) in attrs {
                    let line = lines.line_of(offset);
                    rows.push(IndexRow::new(
                        &attr_section,
                        attr,
                        &clip(value, MAX_VALUE_CHARS),
                        line,
                        line,
                    ));
                }
                if !self_closing {
                    stack.push(Open {
                        name: name.to_string(),
                        segment: seg,
                        row,
                        counts: HashMap::new(),
                    });
                }
            }
            Tok::End { name, end } => {
                let Some(pos) = stack.iter().rposition(|o| o.name == name) else {
                    continue;
                };
                let line = lines.last_line(block.start, end);
                for open in stack.drain(pos..) {
                    rows[open.row].line_end = line;
                }
            }
        }
    }
    let tail =
        unclosed_end_line.unwrap_or_else(|| lines.last_line(block.start, block.end));
    for open in stack {
        rows[open.row].line_end = tail;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r#"<?xml version="1.0"?>
<!-- comment <g> -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
  <g id="layer">
    <path d="M0 0h16"/>
    <path
      d="M1 1"
      fill='red'/>
  </g>
  <![CDATA[ <fake/> ]]>
  <rect width="4"/>
</svg>
"#;

    #[test]
    fn plans_root_children() {
        let lines = LineIndex::new(SVG);
        let blocks = plan(SVG, &lines);
        let sections: Vec<_> = blocks.iter().map(|b| b.section.as_str()).collect();
        assert_eq!(sections, vec!["svg", "svg/g[0]", "svg/rect[0]"]);
    }

    #[test]
    fn element_and_attribute_rows() {
        let lines = LineIndex::new(SVG);
        let rows: Vec<_> = plan(SVG, &lines)
            .iter()
            .flat_map(|b| analyze(SVG, &lines, b))
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
                ("svg", "svg", "depth=0 attrs=2", 2, 11),
                ("svg@attrs", "xmlns", "http://www.w3.org/2000/svg", 2, 2),
                ("svg@attrs", "viewBox", "0 0 16 16", 2, 2),
                ("svg/g[0]", "g#layer", "depth=1 attrs=1", 3, 8),
                ("svg/g[0]@attrs", "id", "layer", 3, 3),
                ("svg/g[0]/path[0]", "path", "depth=2 attrs=1", 4, 4),
                ("svg/g[0]/path[0]@attrs", "d", "M0 0h16", 4, 4),
                ("svg/g[0]/path[1]", "path", "depth=2 attrs=2", 5, 7),
                ("svg/g[0]/path[1]@attrs", "d", "M1 1", 6, 6),
                ("svg/g[0]/path[1]@attrs", "fill", "red", 7, 7),
                ("svg/rect[0]", "rect", "depth=1 attrs=1", 10, 10),
                ("svg/rect[0]@attrs", "width", "4", 10, 10),
            ]
        );
    }

    #[test]
    fn xaml_names() {
        let src = "<Window x:Class=\"A\">\n<Grid>\n<Button x:Name=\"Ok\" />\n</Grid>\n</Window>";
        let lines = LineIndex::new(src);
        let rows: Vec<_> = plan(src, &lines)
            .iter()
            .flat_map(|b| analyze(src, &lines, b))
            .collect();
        let button = rows.iter().find(|r| r.key == "Button#Ok").unwrap();
        assert_eq!(button.section, "Window/Grid[0]/Button[0]");
        assert_eq!(button.value, "depth=2 attrs=1");
    }
}
