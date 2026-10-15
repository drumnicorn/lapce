//! Extension dispatch to the per-type analyzers.
//!
//! Text formats are processed in two phases so the queue can run them
//! incrementally: [`plan`] splits a file into [`Block`]s (a toml `[section]`,
//! a top-level Rust item, a top-level XML child element, a top-level JSON
//! member) and [`analyze`] turns one block into [`IndexRow`]s. gguf is binary
//! and is driven by [`gguf::step`] instead.

pub mod gguf;
mod json;
mod markup;
mod rust;
mod toml;

use crate::index::IndexRow;

pub const MAX_VALUE_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Toml,
    Rust,
    Svg,
    Xml,
    Xaml,
    Json,
    Gguf,
}

impl FileKind {
    pub const ALL: [FileKind; 7] = [
        FileKind::Toml,
        FileKind::Rust,
        FileKind::Svg,
        FileKind::Xml,
        FileKind::Xaml,
        FileKind::Json,
        FileKind::Gguf,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            FileKind::Toml => "toml",
            FileKind::Rust => "rs",
            FileKind::Svg => "svg",
            FileKind::Xml => "xml",
            FileKind::Xaml => "xaml",
            FileKind::Json => "json",
            FileKind::Gguf => "gguf",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.to_ascii_lowercase();
        Self::ALL.into_iter().find(|kind| kind.extension() == ext)
    }

    pub fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next()?;
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        Self::from_extension(ext)
    }
}

/// A byte range of a file that is analyzed as one queue task.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub section: String,
    pub start: usize,
    pub end: usize,
    pub ctx: BlockCtx,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum BlockCtx {
    #[default]
    None,
    Markup {
        depth: usize,
        parent: Vec<String>,
        index: usize,
        /// Line where elements left open at the end of the block are closed.
        unclosed_end_line: Option<usize>,
    },
    Json {
        path: String,
    },
}

/// Maps byte offsets to 0-based line numbers.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    pub fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// Line of the last byte of a non-empty `start..end` range.
    pub fn last_line(&self, start: usize, end: usize) -> usize {
        self.line_of(end.max(start + 1) - 1)
    }
}

/// Iterates `(line_no, byte_offset, line_without_newline)` over a range.
pub fn lines_in<'a>(
    text: &'a str,
    lines: &LineIndex,
    start: usize,
    end: usize,
) -> impl Iterator<Item = (usize, usize, &'a str)> {
    let first = lines.line_of(start);
    let mut offset = start;
    text[start..end]
        .split_inclusive('\n')
        .enumerate()
        .map(move |(i, raw)| {
            let at = offset;
            offset += raw.len();
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            (first + i, at, line.strip_suffix('\r').unwrap_or(line))
        })
}

pub fn clip(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

pub fn plan(kind: FileKind, text: &str, lines: &LineIndex) -> Vec<Block> {
    match kind {
        FileKind::Toml => toml::plan(text, lines),
        FileKind::Rust => rust::plan(text, lines),
        FileKind::Svg | FileKind::Xml | FileKind::Xaml => markup::plan(text, lines),
        FileKind::Json => json::plan(text, lines),
        FileKind::Gguf => Vec::new(),
    }
}

pub fn analyze(
    kind: FileKind,
    text: &str,
    lines: &LineIndex,
    block: &Block,
) -> Vec<IndexRow> {
    match kind {
        FileKind::Toml => toml::analyze(text, lines, block),
        FileKind::Rust => rust::analyze(text, lines, block),
        FileKind::Svg | FileKind::Xml | FileKind::Xaml => {
            markup::analyze(text, lines, block)
        }
        FileKind::Json => json::analyze(text, lines, block),
        FileKind::Gguf => Vec::new(),
    }
}

/// Item keyword of a Rust row, used to pick a symbol kind.
pub fn rust_item_keyword(row: &IndexRow) -> Option<&'static str> {
    rust::item_keyword(&row.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_path() {
        assert_eq!(FileKind::from_path("/a/b/Cargo.toml"), Some(FileKind::Toml));
        assert_eq!(FileKind::from_path("C:\\x\\y.XAML"), Some(FileKind::Xaml));
        assert_eq!(
            FileKind::from_path("model.Q4_K_M.gguf"),
            Some(FileKind::Gguf)
        );
        assert_eq!(FileKind::from_path("/a/.json"), None);
        assert_eq!(FileKind::from_path("/a/b.txt"), None);
    }

    #[test]
    fn line_index() {
        let text = "a\nbc\n\nd";
        let lines = LineIndex::new(text);
        assert_eq!(lines.line_of(0), 0);
        assert_eq!(lines.line_of(2), 1);
        assert_eq!(lines.line_of(5), 2);
        assert_eq!(lines.line_of(6), 3);
        let collected: Vec<_> = lines_in(text, &lines, 2, text.len()).collect();
        assert_eq!(collected, vec![(1, 2, "bc"), (2, 5, ""), (3, 6, "d")]);
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("  héllo ", 3), "hél…");
        assert_eq!(clip("abc", 3), "abc");
    }
}
