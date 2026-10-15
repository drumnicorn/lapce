//! Sub-task queue. Every file is split into blocks (toml `[section]`s, Rust
//! top-level items, XML root children, JSON top-level members, gguf entry
//! batches) and each block is its own task, so progress can be reported at
//! block granularity through `$/progress`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{json, Value};

use crate::index::Index;
use crate::scanner::{self, gguf, Block, FileKind, LineIndex};

pub const MAX_ROWS_PER_FILE: usize = 50_000;
const GGUF_BUDGET: usize = 256;
const REPORT_EVERY: usize = 32;

/// Host services: file listing and ranged reads (via `host/executeProcess`
/// in the plugin, plain `std::fs` in tests) plus progress/log output.
pub trait Host {
    fn list_files(&mut self, root: &str) -> Result<Vec<String>, String>;
    fn read_range(
        &mut self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, String>;
    fn progress(&mut self, token: &str, value: Value);
    fn create_progress(&mut self, _token: &str) {}
    fn log(&mut self, message: &str);
}

pub struct SourceFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub text: String,
    pub lines: LineIndex,
}

pub enum Task {
    Discover {
        root: String,
    },
    File {
        path: String,
        kind: FileKind,
    },
    Block {
        file: Rc<SourceFile>,
        block: Block,
        ordinal: usize,
        of: usize,
    },
    Gguf {
        path: Rc<str>,
        cursor: gguf::Cursor,
    },
}

pub struct TaskQueue {
    tasks: VecDeque<Task>,
    pub max_file_bytes: u64,
    files_total: usize,
    files_done: usize,
    token: Option<String>,
    runs: usize,
    since_report: usize,
    last_percentage: u32,
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self {
            tasks: VecDeque::new(),
            max_file_bytes: 2 << 20,
            files_total: 0,
            files_done: 0,
            token: None,
            runs: 0,
            since_report: 0,
            last_percentage: 0,
        }
    }
}

impl TaskQueue {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn enqueue_discover(&mut self, root: &str) {
        self.tasks.push_back(Task::Discover {
            root: root.to_string(),
        });
    }

    pub fn enqueue_file(&mut self, path: &str) -> bool {
        let Some(kind) = FileKind::from_path(path) else {
            return false;
        };
        self.files_total += 1;
        self.tasks.push_back(Task::File {
            path: path.to_string(),
            kind,
        });
        true
    }

    fn push_front_all(&mut self, tasks: Vec<Task>) {
        for task in tasks.into_iter().rev() {
            self.tasks.push_front(task);
        }
    }

    /// Runs tasks until the queue is empty, reporting progress.
    pub fn run(&mut self, host: &mut dyn Host, index: &mut Index) {
        if self.tasks.is_empty() {
            return;
        }
        self.begin(host);
        while self.step(host, index) {}
        self.end(host, index);
    }

    /// Runs a single task. Returns whether tasks remain.
    pub fn step(&mut self, host: &mut dyn Host, index: &mut Index) -> bool {
        let Some(task) = self.tasks.pop_front() else {
            return false;
        };
        let label = match task {
            Task::Discover { root } => {
                self.discover(host, &root);
                "listing files".to_string()
            }
            Task::File { path, kind } => {
                let label = path.clone();
                let next = self.open_file(host, index, path, kind);
                if next.is_empty() {
                    self.files_done += 1;
                }
                self.push_front_all(next);
                label
            }
            Task::Block {
                file,
                block,
                ordinal,
                of,
            } => {
                let rows =
                    scanner::analyze(file.kind, &file.text, &file.lines, &block);
                push_capped(host, index, &file.path, rows);
                if ordinal + 1 == of {
                    self.files_done += 1;
                }
                format!(
                    "{} [{}] ({}/{of})",
                    file.path.display(),
                    block.section,
                    ordinal + 1
                )
            }
            Task::Gguf { path, mut cursor } => {
                let section = cursor.section();
                let p = PathBuf::from(&*path);
                let mut read = |off: u64, len: u64| host_read(host, &path, off, len);
                match gguf::step(&mut read, &mut cursor, GGUF_BUDGET) {
                    Ok(rows) => {
                        push_capped(host, index, &p, rows);
                    }
                    Err(err) => {
                        host.log(&format!("{}: {err}", p.display()));
                        let row = crate::index::IndexRow::new(
                            section, "!error", &err, 0, 0,
                        );
                        index.push_rows(&p, vec![row]);
                        cursor.stage = gguf::Stage::Done;
                    }
                }
                let label = format!("{} [{section}]", p.display());
                if cursor.is_done() {
                    self.files_done += 1;
                } else {
                    self.tasks.push_front(Task::Gguf { path, cursor });
                }
                label
            }
        };
        self.report(host, &label);
        !self.tasks.is_empty()
    }

    fn discover(&mut self, host: &mut dyn Host, root: &str) {
        match host.list_files(root) {
            Ok(mut files) => {
                files.sort();
                files.dedup();
                for file in files {
                    self.enqueue_file(&file);
                }
            }
            Err(err) => host.log(&format!("failed to list {root}: {err}")),
        }
    }

    fn open_file(
        &mut self,
        host: &mut dyn Host,
        index: &mut Index,
        path: String,
        kind: FileKind,
    ) -> Vec<Task> {
        let p = PathBuf::from(&path);
        index.clear_file(&p);
        if kind == FileKind::Gguf {
            index.touch(&p);
            return vec![Task::Gguf {
                path: path.into(),
                cursor: Default::default(),
            }];
        }
        let bytes = match host_read(host, &path, 0, self.max_file_bytes + 1) {
            Ok(bytes) => bytes,
            Err(err) => {
                host.log(&format!("{path}: {err}"));
                return Vec::new();
            }
        };
        index.touch(&p);
        if bytes.len() as u64 > self.max_file_bytes {
            host.log(&format!(
                "{path}: larger than {} bytes, skipped",
                self.max_file_bytes
            ));
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
        let lines = LineIndex::new(&text);
        let blocks = scanner::plan(kind, &text, &lines);
        let file = Rc::new(SourceFile {
            path: p,
            kind,
            text,
            lines,
        });
        let of = blocks.len();
        blocks
            .into_iter()
            .enumerate()
            .map(|(ordinal, block)| Task::Block {
                file: file.clone(),
                block,
                ordinal,
                of,
            })
            .collect()
    }

    fn percentage(&self) -> u32 {
        if self.files_total == 0 {
            return 0;
        }
        ((self.files_done * 100) / self.files_total).min(100) as u32
    }

    fn begin(&mut self, host: &mut dyn Host) {
        self.runs += 1;
        let token = format!("volt-indexer/{}", self.runs);
        host.create_progress(&token);
        host.progress(
            &token,
            json!({
                "kind": "begin",
                "title": "Indexing",
                "cancellable": false,
                "message": match self.files_total - self.files_done {
                    0 => "listing files".to_string(),
                    n => format!("{n} files queued"),
                },
                "percentage": self.percentage(),
            }),
        );
        self.token = Some(token);
        self.since_report = 0;
    }

    fn report(&mut self, host: &mut dyn Host, label: &str) {
        let Some(token) = self.token.clone() else {
            return;
        };
        self.since_report += 1;
        let percentage = self.percentage();
        if percentage == self.last_percentage && self.since_report < REPORT_EVERY {
            return;
        }
        self.since_report = 0;
        self.last_percentage = percentage;
        host.progress(
            &token,
            json!({
                "kind": "report",
                "message": format!("{}/{} {label}", self.files_done, self.files_total),
                "percentage": percentage,
            }),
        );
    }

    fn end(&mut self, host: &mut dyn Host, index: &Index) {
        if let Some(token) = self.token.take() {
            host.progress(
                &token,
                json!({
                    "kind": "end",
                    "message": format!(
                        "{} files, {} rows",
                        index.file_count(),
                        index.row_count()
                    ),
                }),
            );
        }
        self.files_total = 0;
        self.files_done = 0;
        self.last_percentage = 0;
    }
}

fn host_read(
    host: &mut dyn Host,
    path: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    host.read_range(path, offset, len)
}

fn push_capped(
    host: &mut dyn Host,
    index: &mut Index,
    path: &std::path::Path,
    mut rows: Vec<crate::index::IndexRow>,
) {
    let have = index.rows(path).map_or(0, <[_]>::len);
    if have >= MAX_ROWS_PER_FILE {
        return;
    }
    if have + rows.len() > MAX_ROWS_PER_FILE {
        rows.truncate(MAX_ROWS_PER_FILE - have);
        host.log(&format!(
            "{}: more than {MAX_ROWS_PER_FILE} rows, truncated",
            path.display()
        ));
    }
    index.push_rows(path, rows);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom};

    #[derive(Default)]
    struct FsHost {
        progress: Vec<Value>,
    }

    impl Host for FsHost {
        fn list_files(&mut self, root: &str) -> Result<Vec<String>, String> {
            fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
                for entry in fs::read_dir(dir).unwrap().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else {
                        out.push(path.to_string_lossy().into_owned());
                    }
                }
            }
            let mut out = Vec::new();
            walk(std::path::Path::new(root), &mut out);
            Ok(out)
        }

        fn read_range(
            &mut self,
            path: &str,
            offset: u64,
            len: u64,
        ) -> Result<Vec<u8>, String> {
            let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
            f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.take(len)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            Ok(buf)
        }

        fn progress(&mut self, _token: &str, value: Value) {
            self.progress.push(value);
        }

        fn log(&mut self, _message: &str) {}
    }

    #[test]
    fn indexes_a_workspace() {
        let dir = std::env::temp_dir()
            .join(format!("volt-indexer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/lib.rs"),
            "pub struct A;\nimpl A {\n    fn go(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(dir.join("data.json"), "{\"a\": {\"b\": 1}}").unwrap();
        fs::write(dir.join("icon.svg"), "<svg><rect id=\"r\"/></svg>").unwrap();
        fs::write(dir.join("App.xaml"), "<Window><Grid/></Window>").unwrap();
        fs::write(
            dir.join("model.gguf"),
            crate::scanner::gguf::tests::sample(),
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let mut host = FsHost::default();
        let mut index = Index::default();
        let mut queue = TaskQueue::default();
        queue.enqueue_discover(dir.to_str().unwrap());
        queue.run(&mut host, &mut index);

        assert!(queue.is_empty());
        assert_eq!(index.file_count(), 6);
        let rows = index.rows(&dir.join("Cargo.toml")).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.row_no).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let rs = index.rows(&dir.join("src/lib.rs")).unwrap();
        assert_eq!(
            rs.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["A", "impl A", "go"]
        );
        assert_eq!(index.rows(&dir.join("model.gguf")).unwrap().len(), 12);
        assert_eq!(host.progress.first().unwrap()["kind"], "begin");
        assert_eq!(host.progress.last().unwrap()["kind"], "end");
        assert!(host
            .progress
            .iter()
            .any(|p| p["kind"] == "report" && p["percentage"] == 100));

        // Re-indexing a single file replaces its rows.
        fs::write(dir.join("data.json"), "{\"z\": 2}").unwrap();
        queue.enqueue_file(dir.join("data.json").to_str().unwrap());
        queue.run(&mut host, &mut index);
        let json = index.rows(&dir.join("data.json")).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0].key, "z");

        let symbols = index.workspace_symbols("go", 10);
        assert_eq!(symbols[0]["name"], "go");
        let _ = fs::remove_dir_all(&dir);
    }
}
