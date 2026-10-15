//! gguf: parses the binary header, the metadata key/value section and the
//! tensor info table. The file is read through ranged reads so multi-GB models
//! never have to be loaded; tensor data is never touched.
//!
//! Rows use `line_start = line_end = 0`; the byte offset of each entry is
//! appended to its value as `@0x…` instead.

use crate::index::IndexRow;

use super::{clip, MAX_VALUE_CHARS};

const MAX_STRING: u64 = 16 << 20;
const MAX_ARRAY_PREVIEW: usize = 8;
const MAX_TENSOR_ROWS: u64 = 20_000;
const MAX_COUNT: u64 = 1 << 24;
pub const CHUNK: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Stage {
    #[default]
    Header,
    Metadata,
    Tensors,
    Done,
}

/// Resumable parser position; each queue task advances it by a budget.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub stage: Stage,
    offset: u64,
    big_endian: bool,
    version: u32,
    remaining: u64,
    index: u64,
    tensor_count: u64,
    alignment: u64,
}

impl Cursor {
    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    pub fn section(&self) -> &'static str {
        match self.stage {
            Stage::Header => "header",
            Stage::Metadata => "metadata",
            Stage::Tensors | Stage::Done => "tensors",
        }
    }
}

pub type ReadRange<'a> = dyn FnMut(u64, u64) -> Result<Vec<u8>, String> + 'a;

/// Sequential reader over ranged reads with a one-chunk cache.
struct Reader<'a, 'b> {
    read: &'a mut ReadRange<'b>,
    pos: u64,
    chunk_start: u64,
    chunk: Vec<u8>,
    big_endian: bool,
    wide: bool,
}

impl Reader<'_, '_> {
    fn bytes(&mut self, n: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(n.min(CHUNK) as usize);
        let mut need = n;
        while need > 0 {
            let chunk_end = self.chunk_start + self.chunk.len() as u64;
            if self.pos < self.chunk_start || self.pos >= chunk_end {
                let start = self.pos - self.pos % CHUNK;
                self.chunk = (self.read)(start, CHUNK.max(need.min(16 * CHUNK)))?;
                self.chunk_start = start;
                if self.pos >= start + self.chunk.len() as u64 {
                    return Err(format!("unexpected end of file at {}", self.pos));
                }
            }
            let at = (self.pos - self.chunk_start) as usize;
            let take = need.min((self.chunk.len() - at) as u64) as usize;
            out.extend_from_slice(&self.chunk[at..at + take]);
            self.pos += take as u64;
            need -= take as u64;
        }
        Ok(out)
    }

    fn skip(&mut self, n: u64) {
        self.pos += n;
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut out = [0; N];
        out.copy_from_slice(&self.bytes(N as u64)?);
        if self.big_endian {
            out.reverse();
        }
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// Counts and string lengths are u32 in gguf v1 and u64 afterwards.
    fn count(&mut self) -> Result<u64, String> {
        if self.wide {
            self.u64()
        } else {
            Ok(self.u32()? as u64)
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let len = self.count()?;
        if len > MAX_STRING {
            return Err(format!("string of {len} bytes at {}", self.pos));
        }
        Ok(String::from_utf8_lossy(&self.bytes(len)?).into_owned())
    }

    fn scalar(&mut self, ty: u32) -> Result<String, String> {
        Ok(match ty {
            0 => self.u8()?.to_string(),
            1 => (self.u8()? as i8).to_string(),
            2 => self.u16()?.to_string(),
            3 => (self.u16()? as i16).to_string(),
            4 => self.u32()?.to_string(),
            5 => (self.u32()? as i32).to_string(),
            6 => f32::from_bits(self.u32()?).to_string(),
            7 => (self.u8()? != 0).to_string(),
            8 => self.string()?,
            10 => self.u64()?.to_string(),
            11 => (self.u64()? as i64).to_string(),
            12 => f64::from_bits(self.u64()?).to_string(),
            _ => return Err(format!("unknown value type {ty}")),
        })
    }

    fn value(&mut self, ty: u32) -> Result<String, String> {
        if ty != 9 {
            return self.scalar(ty);
        }
        let item_ty = self.u32()?;
        let len = self.count()?;
        let mut preview = Vec::new();
        if let Some(size) = fixed_size(item_ty) {
            for _ in 0..len.min(MAX_ARRAY_PREVIEW as u64) {
                preview.push(self.scalar(item_ty)?);
            }
            self.skip(len.saturating_sub(MAX_ARRAY_PREVIEW as u64) * size);
        } else if item_ty == 8 {
            for i in 0..len {
                let s = self.string()?;
                if (i as usize) < MAX_ARRAY_PREVIEW {
                    preview.push(format!("{s:?}"));
                }
            }
        } else {
            return Err(format!("unsupported array item type {item_ty}"));
        }
        let more = if len as usize > preview.len() {
            ", …"
        } else {
            ""
        };
        Ok(format!(
            "[{}; {len}] [{}{more}]",
            type_name(item_ty),
            preview.join(", ")
        ))
    }
}

fn fixed_size(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

fn type_name(ty: u32) -> &'static str {
    match ty {
        0 => "u8",
        1 => "i8",
        2 => "u16",
        3 => "i16",
        4 => "u32",
        5 => "i32",
        6 => "f32",
        7 => "bool",
        8 => "string",
        9 => "array",
        10 => "u64",
        11 => "i64",
        12 => "f64",
        _ => "?",
    }
}

fn ggml_type(ty: u32) -> String {
    const NAMES: &[&str] = &[
        "F32", "F16", "Q4_0", "Q4_1", "Q4_2", "Q4_3", "Q5_0", "Q5_1", "Q8_0",
        "Q8_1", "Q2_K", "Q3_K", "Q4_K", "Q5_K", "Q6_K", "Q8_K", "IQ2_XXS", "IQ2_XS",
        "IQ3_XXS", "IQ1_S", "IQ4_NL", "IQ3_S", "IQ2_S", "IQ4_XS", "I8", "I16",
        "I32", "I64", "F64", "IQ1_M", "BF16",
    ];
    NAMES
        .get(ty as usize)
        .map_or_else(|| format!("type{ty}"), |n| n.to_string())
}

/// Advances `cursor` by at most `budget` entries and returns the new rows.
pub fn step(
    read: &mut ReadRange<'_>,
    cursor: &mut Cursor,
    budget: usize,
) -> Result<Vec<IndexRow>, String> {
    let mut r = Reader {
        read,
        pos: cursor.offset,
        chunk_start: 0,
        chunk: Vec::new(),
        big_endian: cursor.big_endian,
        wide: cursor.version != 1,
    };
    let mut rows = Vec::new();
    let row = |section: &str, key: &str, value: &str, at: u64| {
        IndexRow::new(section, key, &format!("{value} @{at:#x}"), 0, 0)
    };

    match cursor.stage {
        Stage::Header => {
            let magic = r.bytes(4)?;
            if magic != b"GGUF" {
                return Err("not a gguf file (bad magic)".to_string());
            }
            let raw = r.bytes(4)?;
            let le = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let be = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let (version, big_endian) = if le & 0xFFFF != 0 {
                (le, false)
            } else {
                (be, true)
            };
            r.big_endian = big_endian;
            r.wide = version != 1;
            let tensor_count = r.count()?;
            let kv_count = r.count()?;
            if tensor_count > MAX_COUNT || kv_count > MAX_COUNT {
                return Err("implausible gguf header counts".to_string());
            }
            let endian = if big_endian { "big" } else { "little" };
            rows.push(row("header", "magic", "GGUF", 0));
            rows.push(row("header", "version", &version.to_string(), 4));
            rows.push(row("header", "endianness", endian, 4));
            rows.push(row("header", "tensor_count", &tensor_count.to_string(), 8));
            rows.push(row("header", "metadata_kv_count", &kv_count.to_string(), 8));
            *cursor = Cursor {
                stage: Stage::Metadata,
                offset: r.pos,
                big_endian,
                version,
                remaining: kv_count,
                index: 0,
                tensor_count,
                alignment: 32,
            };
        }
        Stage::Metadata => {
            while cursor.remaining > 0 && rows.len() < budget {
                let at = r.pos;
                let key = r.string()?;
                let ty = r.u32()?;
                let value = r.value(ty)?;
                if key == "general.alignment" {
                    cursor.alignment = value.parse().unwrap_or(32);
                }
                let shown =
                    format!("{}: {}", type_name(ty), clip(&value, MAX_VALUE_CHARS));
                rows.push(row("metadata", &key, &shown, at));
                cursor.remaining -= 1;
            }
            if cursor.remaining == 0 {
                cursor.stage = Stage::Tensors;
                cursor.remaining = cursor.tensor_count;
                cursor.index = 0;
            }
            cursor.offset = r.pos;
        }
        Stage::Tensors => {
            while cursor.remaining > 0 && rows.len() < budget {
                if cursor.index >= MAX_TENSOR_ROWS {
                    rows.push(row(
                        "tensors",
                        "!truncated",
                        &format!("{} more tensors", cursor.remaining),
                        r.pos,
                    ));
                    cursor.remaining = 0;
                    break;
                }
                let at = r.pos;
                let name = r.string()?;
                let n_dims = r.u32()?;
                if n_dims > 8 {
                    return Err(format!("tensor {name} has {n_dims} dimensions"));
                }
                let dims = (0..n_dims)
                    .map(|_| r.count().map(|d| d.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let ty = r.u32()?;
                let offset = r.u64()?;
                rows.push(row(
                    "tensors",
                    &name,
                    &format!(
                        "{} [{}] data+{offset:#x}",
                        ggml_type(ty),
                        dims.join(", ")
                    ),
                    at,
                ));
                cursor.remaining -= 1;
                cursor.index += 1;
            }
            if cursor.remaining == 0 {
                let data_start = r.pos.div_ceil(cursor.alignment.max(1))
                    * cursor.alignment.max(1);
                rows.push(row(
                    "tensors",
                    "data_start",
                    &format!("alignment={}", cursor.alignment),
                    data_start,
                ));
                cursor.stage = Stage::Done;
            }
            cursor.offset = r.pos;
        }
        Stage::Done => {}
    }
    Ok(rows)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn s(out: &mut Vec<u8>, v: &str) {
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }

    /// A small but structurally complete gguf v3 file.
    pub fn sample() -> Vec<u8> {
        let mut out = b"GGUF".to_vec();
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&2u64.to_le_bytes()); // tensors
        out.extend_from_slice(&4u64.to_le_bytes()); // kv
        s(&mut out, "general.architecture");
        out.extend_from_slice(&8u32.to_le_bytes());
        s(&mut out, "llama");
        s(&mut out, "llama.context_length");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4096u32.to_le_bytes());
        s(&mut out, "tokenizer.ggml.tokens");
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&10u64.to_le_bytes());
        for i in 0..10 {
            s(&mut out, &format!("t{i}"));
        }
        s(&mut out, "tokenizer.ggml.scores");
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&6u32.to_le_bytes());
        out.extend_from_slice(&3u64.to_le_bytes());
        for f in [0.5f32, -1.0, 2.0] {
            out.extend_from_slice(&f.to_le_bytes());
        }
        for (name, ty) in [("token_embd.weight", 1u32), ("output.weight", 0)] {
            s(&mut out, name);
            out.extend_from_slice(&2u32.to_le_bytes());
            out.extend_from_slice(&4096u64.to_le_bytes());
            out.extend_from_slice(&32000u64.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        out
    }

    pub fn run_all(data: &[u8], budget: usize) -> Result<Vec<IndexRow>, String> {
        let mut read = |off: u64, len: u64| -> Result<Vec<u8>, String> {
            let start = (off as usize).min(data.len());
            let end = (start + len as usize).min(data.len());
            Ok(data[start..end].to_vec())
        };
        let mut cursor = Cursor::default();
        let mut rows = Vec::new();
        while !cursor.is_done() {
            rows.extend(step(&mut read, &mut cursor, budget)?);
        }
        Ok(rows)
    }

    #[test]
    fn parses_header_metadata_and_tensors() {
        let rows = run_all(&sample(), 2).unwrap();
        let simple: Vec<_> = rows
            .iter()
            .map(|r| (r.section.as_str(), r.key.as_str(), r.value.as_str()))
            .collect();
        assert_eq!(simple[0], ("header", "magic", "GGUF @0x0"));
        assert_eq!(simple[1], ("header", "version", "3 @0x4"));
        assert_eq!(simple[3], ("header", "tensor_count", "2 @0x8"));
        assert_eq!(simple[5].1, "general.architecture");
        assert!(simple[5].2.starts_with("string: llama @0x18"));
        assert!(simple[6].2.starts_with("u32: 4096 "));
        assert!(simple[7]
            .2
            .starts_with("array: [string; 10] [\"t0\", \"t1\", \"t2\", \"t3\", \"t4\", \"t5\", \"t6\", \"t7\", …]"));
        assert!(simple[8].2.starts_with("array: [f32; 3] [0.5, -1, 2]"));
        assert_eq!(simple[9].1, "token_embd.weight");
        assert!(simple[9].2.starts_with("F16 [4096, 32000] data+0x0"));
        assert!(simple[10].2.starts_with("F32 [4096, 32000]"));
        assert_eq!(simple[11].1, "data_start");
        assert_eq!(rows.len(), 12);
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(run_all(b"NOPE\0\0\0\0", 8).is_err());
        let data = sample();
        assert!(run_all(&data[..60], 8).is_err());
    }
}
