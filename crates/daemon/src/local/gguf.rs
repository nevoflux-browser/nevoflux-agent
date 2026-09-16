//! Minimal GGUF header reader.
//!
//! Reads only the GGUF magic/version/counts and the metadata key-value
//! section that [`crate::local::memory`] needs to estimate a downloaded
//! model's KV-cache and compute memory. Tensor data — the vast majority of
//! the file — is never touched.
//!
//! GGUF (little-endian) layout: 4-byte magic `b"GGUF"`, `u32` version,
//! `u64` tensor_count, `u64` metadata_kv_count, then `metadata_kv_count`
//! key-value pairs. Each pair is a length-prefixed UTF-8 string key
//! followed by a `u32` value-type tag and a value of that type (a scalar,
//! a string, or an array of a single element type). See
//! <https://github.com/ggml-org/ggml/blob/master/docs/gguf.md>.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// GGUF header fields this engine needs for memory estimation and request
/// shaping (Task 2.1's `catalog::GgufHeaderStatic` mirrors this shape as
/// compile-time constants for the known catalog models; this type is for
/// reading it back out of an actual downloaded file, e.g. to verify the
/// catalog against reality).
#[derive(Debug, Clone, PartialEq)]
pub struct GgufHeader {
    pub architecture: String,
    pub block_count: u32,
    pub embedding_length: u32,
    pub head_count: u32,
    pub head_count_kv: u32,
    pub key_length: u32,
    pub context_length: u32,
}

/// GGUF metadata value types (`gguf_metadata_value_type` in the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    Bool,
    String,
    Array,
    U64,
    I64,
    F64,
}

impl ValueType {
    fn from_u32(v: u32) -> Result<Self, String> {
        Ok(match v {
            0 => Self::U8,
            1 => Self::I8,
            2 => Self::U16,
            3 => Self::I16,
            4 => Self::U32,
            5 => Self::I32,
            6 => Self::F32,
            7 => Self::Bool,
            8 => Self::String,
            9 => Self::Array,
            10 => Self::U64,
            11 => Self::I64,
            12 => Self::F64,
            other => return Err(format!("unknown GGUF metadata value type {other}")),
        })
    }

    /// The minimum encoded size of a value of this type: exact for every
    /// fixed-width scalar, and the size of an *empty* instance for the two
    /// variable-length types (an empty string is still an 8-byte length
    /// prefix; a zero-element array is still a 4-byte element-type tag plus
    /// an 8-byte count). Used to sanity-check an array's declared element
    /// count against the file's remaining size before looping over it.
    fn min_encoded_size(self) -> u64 {
        match self {
            Self::U8 | Self::I8 | Self::Bool => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
            Self::String => 8,
            Self::Array => 12,
        }
    }
}

/// Upper bound on a single length-prefixed read (a metadata string, or an
/// array's backing bytes). Real GGUF metadata values are at most a few
/// KiB — this reader never reads tensor data, which is the only part of a
/// GGUF file that legitimately gets large — so anything claiming more is
/// almost certainly a corrupted length field, not a real file. Checked
/// before any allocation.
const MAX_SINGLE_READ_BYTES: u64 = 16 * 1024 * 1024;

/// Thin reader over any `Read` that knows the GGUF primitive encodings.
///
/// Tracks `remaining`, the count of unconsumed bytes left in the file, so
/// every length or element-count read from file data — never trustworthy,
/// since it can be an arbitrary `u64`/`u32` from a corrupted or truncated
/// file — is validated against both the file's actual remaining size and
/// [`MAX_SINGLE_READ_BYTES`] *before* anything is allocated. Without this,
/// a single flipped bit near `u64::MAX` in a length field would attempt a
/// multi-exabyte allocation: a capacity overflow or allocator abort that
/// `Result<_, String>` can't catch, i.e. exactly the kind of input this
/// reader must treat as untrusted (it's parsing a downloaded file).
struct Reader<R: Read> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Reader<R> {
    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let n64 = n as u64;
        if n64 > MAX_SINGLE_READ_BYTES {
            return Err(format!(
                "GGUF metadata declares a {n} byte value, over the \
                 {MAX_SINGLE_READ_BYTES} byte sanity ceiling for a single value \
                 (likely a corrupted length field)"
            ));
        }
        self.ensure_remaining(n64)?;
        let mut buf = vec![0u8; n];
        self.inner
            .read_exact(&mut buf)
            .map_err(|e| format!("unexpected end of GGUF file: {e}"))?;
        self.remaining -= n64;
        Ok(buf)
    }

    /// Reject `needed` up front (before allocating or looping) if the file
    /// doesn't have that many bytes left, per this struct's doc comment.
    fn ensure_remaining(&self, needed: u64) -> Result<(), String> {
        if needed > self.remaining {
            Err(format!(
                "GGUF metadata declares a length/count needing {needed} bytes \
                 but only {} bytes remain in the file",
                self.remaining
            ))
        } else {
            Ok(())
        }
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn read_value_type(&mut self) -> Result<ValueType, String> {
        ValueType::from_u32(self.read_u32()?)
    }

    fn read_string(&mut self) -> Result<String, String> {
        let len = self.read_u64()? as usize;
        let bytes = self.read_bytes(len)?;
        String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8 in GGUF string: {e}"))
    }

    /// Read and discard a value of the given type, recursing into array
    /// elements. Used both for metadata this reader doesn't care about and
    /// for consuming an array's element-type/count header plus its
    /// elements once the outer `Array` tag has already been read.
    fn skip_value(&mut self, vtype: ValueType) -> Result<(), String> {
        match vtype {
            ValueType::U8 | ValueType::I8 | ValueType::Bool => {
                self.read_bytes(1)?;
            }
            ValueType::U16 | ValueType::I16 => {
                self.read_bytes(2)?;
            }
            ValueType::U32 | ValueType::I32 | ValueType::F32 => {
                self.read_bytes(4)?;
            }
            ValueType::U64 | ValueType::I64 | ValueType::F64 => {
                self.read_bytes(8)?;
            }
            ValueType::String => {
                self.read_string()?;
            }
            ValueType::Array => {
                let elem_type = self.read_value_type()?;
                let count = self.read_u64()?;
                // Bound the declared count against the file's remaining
                // size before looping — a corrupted count (e.g. near
                // `u64::MAX`) would otherwise spin through this loop
                // billions of times before `skip_value` eventually errors
                // out from real EOF. `checked_mul` treats an overflowing
                // "minimum bytes needed" as unconditionally too large
                // (`u64::MAX`), which always fails `ensure_remaining`.
                let min_needed = count
                    .checked_mul(elem_type.min_encoded_size())
                    .unwrap_or(u64::MAX);
                self.ensure_remaining(min_needed)?;
                for _ in 0..count {
                    self.skip_value(elem_type)?;
                }
            }
        }
        Ok(())
    }

    /// Read a scalar numeric value, widened to `u64`. Errors on `String`
    /// and `Array` — every metadata key this reader extracts by number
    /// (`*_count`, `*_length`) is a scalar integer in real GGUF files.
    fn read_scalar_as_u64(&mut self, vtype: ValueType) -> Result<u64, String> {
        Ok(match vtype {
            ValueType::U8 | ValueType::Bool => self.read_bytes(1)?[0] as u64,
            ValueType::I8 => self.read_bytes(1)?[0] as i8 as i64 as u64,
            ValueType::U16 => u16::from_le_bytes(self.read_bytes(2)?.try_into().unwrap()) as u64,
            ValueType::I16 => {
                i16::from_le_bytes(self.read_bytes(2)?.try_into().unwrap()) as i64 as u64
            }
            ValueType::U32 => self.read_u32()? as u64,
            ValueType::I32 => {
                i32::from_le_bytes(self.read_bytes(4)?.try_into().unwrap()) as i64 as u64
            }
            ValueType::F32 => f32::from_le_bytes(self.read_bytes(4)?.try_into().unwrap()) as u64,
            ValueType::U64 => self.read_u64()?,
            ValueType::I64 => i64::from_le_bytes(self.read_bytes(8)?.try_into().unwrap()) as u64,
            ValueType::F64 => f64::from_le_bytes(self.read_bytes(8)?.try_into().unwrap()) as u64,
            ValueType::String | ValueType::Array => {
                return Err("expected a numeric GGUF value, found a string or array".to_string());
            }
        })
    }
}

/// Read a GGUF file's header and the dimension fields under
/// `general.architecture` and `{architecture}.*` from its metadata
/// key-value section. Only GGUF version 3 is supported. Tensor data is
/// never read.
pub fn read_header(path: &Path) -> Result<GgufHeader, String> {
    let file = File::open(path).map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let total_len = file
        .metadata()
        .map_err(|e| format!("failed to stat {}: {e}", path.display()))?
        .len();
    let mut r = Reader {
        inner: BufReader::new(file),
        remaining: total_len,
    };

    let magic = r.read_bytes(4)?;
    if magic != b"GGUF" {
        return Err(format!(
            "not a GGUF file: expected magic b\"GGUF\", found {magic:02x?}"
        ));
    }
    let version = r.read_u32()?;
    if version != 3 {
        return Err(format!(
            "unsupported GGUF version {version}: only version 3 is supported"
        ));
    }
    let _tensor_count = r.read_u64()?;
    let kv_count = r.read_u64()?;

    let mut numeric: HashMap<String, u64> = HashMap::new();
    let mut strings: HashMap<String, String> = HashMap::new();

    for _ in 0..kv_count {
        let key = r.read_string()?;
        let vtype = r.read_value_type()?;
        match vtype {
            ValueType::String => {
                let value = r.read_string()?;
                strings.insert(key, value);
            }
            ValueType::Array => {
                // No metadata key this reader extracts is an array
                // (tokenizer vocab/scores etc. are, but we don't need
                // them) — read and discard.
                r.skip_value(ValueType::Array)?;
            }
            other => {
                let value = r.read_scalar_as_u64(other)?;
                numeric.insert(key, value);
            }
        }
    }

    let architecture = strings
        .get("general.architecture")
        .cloned()
        .ok_or_else(|| "missing GGUF metadata key general.architecture".to_string())?;

    let numeric_key = |suffix: &str| -> Result<u32, String> {
        let key = format!("{architecture}.{suffix}");
        numeric
            .get(&key)
            .copied()
            .map(|v| v as u32)
            .ok_or_else(|| format!("missing GGUF metadata key {key:?}"))
    };

    let block_count = numeric_key("block_count")?;
    let embedding_length = numeric_key("embedding_length")?;
    let head_count = numeric_key("attention.head_count")?;
    let head_count_kv = numeric_key("attention.head_count_kv")?;
    let key_length = match numeric.get(&format!("{architecture}.attention.key_length")) {
        Some(v) => *v as u32,
        // `embedding_length / head_count` is only a valid stand-in for
        // per-head key size under true multi-head attention, where every
        // head has its own K/V projection. Under grouped-query attention
        // (head_count_kv < head_count) it silently computes the wrong
        // number — e.g. qwen3-4b-instruct-2507's real key_length is 128,
        // but embedding_length(2560)/head_count(32) is 80 — so only take
        // this fallback when head_count == head_count_kv; otherwise there
        // is no way to infer key_length from the other fields, and we
        // must error rather than return a plausible-looking wrong value.
        None if head_count > 0 && head_count == head_count_kv => embedding_length / head_count,
        None => {
            return Err(format!(
                "{architecture}.attention.key_length missing and cannot be inferred \
                 for grouped-query attention (head_count={head_count}, \
                 head_count_kv={head_count_kv})"
            ));
        }
    };
    let context_length = numeric_key("context_length")?;

    Ok(GgufHeader {
        architecture,
        block_count,
        embedding_length,
        head_count,
        head_count_kv,
        key_length,
        context_length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn push_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn push_string_kv(buf: &mut Vec<u8>, key: &str, value: &str) {
        push_u64(buf, key.len() as u64);
        buf.extend_from_slice(key.as_bytes());
        push_u32(buf, 8); // ValueType::String
        push_u64(buf, value.len() as u64);
        buf.extend_from_slice(value.as_bytes());
    }
    fn push_u32_kv(buf: &mut Vec<u8>, key: &str, value: u32) {
        push_u64(buf, key.len() as u64);
        buf.extend_from_slice(key.as_bytes());
        push_u32(buf, 4); // ValueType::U32
        push_u32(buf, value);
    }

    fn write_temp_gguf(kv_pairs: usize, body: &[u8]) -> tempfile::NamedTempFile {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        push_u32(&mut buf, 3); // version
        push_u64(&mut buf, 0); // tensor_count
        push_u64(&mut buf, kv_pairs as u64); // metadata_kv_count
        buf.extend_from_slice(body);

        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&buf).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_a_synthetic_gguf_header() {
        let mut body = Vec::new();
        push_string_kv(&mut body, "general.architecture", "qwen3");
        push_u32_kv(&mut body, "qwen3.block_count", 36);
        push_u32_kv(&mut body, "qwen3.embedding_length", 2560);
        push_u32_kv(&mut body, "qwen3.attention.head_count", 32);
        push_u32_kv(&mut body, "qwen3.attention.head_count_kv", 8);
        push_u32_kv(&mut body, "qwen3.attention.key_length", 128);
        push_u32_kv(&mut body, "qwen3.context_length", 262144);

        let f = write_temp_gguf(7, &body);
        let header = read_header(f.path()).unwrap();

        assert_eq!(
            header,
            GgufHeader {
                architecture: "qwen3".to_string(),
                block_count: 36,
                embedding_length: 2560,
                head_count: 32,
                head_count_kv: 8,
                key_length: 128,
                context_length: 262144,
            }
        );
    }

    #[test]
    fn falls_back_to_embedding_over_head_count_under_true_mha() {
        // head_count == head_count_kv: true multi-head attention, where
        // embedding_length/head_count is a valid stand-in for key_length.
        let mut body = Vec::new();
        push_string_kv(&mut body, "general.architecture", "qwen3");
        push_u32_kv(&mut body, "qwen3.block_count", 28);
        push_u32_kv(&mut body, "qwen3.embedding_length", 2048);
        push_u32_kv(&mut body, "qwen3.attention.head_count", 16);
        push_u32_kv(&mut body, "qwen3.attention.head_count_kv", 16);
        push_u32_kv(&mut body, "qwen3.context_length", 40960);

        let f = write_temp_gguf(6, &body);
        let header = read_header(f.path()).unwrap();
        assert_eq!(header.key_length, 2048 / 16);
    }

    #[test]
    fn errors_on_missing_key_length_under_grouped_query_attention() {
        // head_count (32) != head_count_kv (8): grouped-query attention,
        // as in the real qwen3-4b-instruct-2507 (embedding_length 2560,
        // head_count 32, real key_length 128). embedding_length/head_count
        // would silently compute 80 here — wrong — so this must error
        // instead of falling back.
        let mut body = Vec::new();
        push_string_kv(&mut body, "general.architecture", "qwen3");
        push_u32_kv(&mut body, "qwen3.block_count", 36);
        push_u32_kv(&mut body, "qwen3.embedding_length", 2560);
        push_u32_kv(&mut body, "qwen3.attention.head_count", 32);
        push_u32_kv(&mut body, "qwen3.attention.head_count_kv", 8);
        push_u32_kv(&mut body, "qwen3.context_length", 262144);

        let f = write_temp_gguf(6, &body);
        let err = read_header(f.path()).unwrap_err();
        assert!(
            err.contains("grouped-query attention"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn skips_array_metadata_it_does_not_need() {
        let mut body = Vec::new();
        push_string_kv(&mut body, "general.architecture", "qwen3");
        // tokenizer.ggml.tokens: ARRAY of STRING, 3 elements.
        push_u64(&mut body, "tokenizer.ggml.tokens".len() as u64);
        body.extend_from_slice(b"tokenizer.ggml.tokens");
        push_u32(&mut body, 9); // ValueType::Array
        push_u32(&mut body, 8); // element type: String
        push_u64(&mut body, 3); // count
        for tok in ["<pad>", "<eos>", "hi"] {
            push_u64(&mut body, tok.len() as u64);
            body.extend_from_slice(tok.as_bytes());
        }
        push_u32_kv(&mut body, "qwen3.block_count", 36);
        push_u32_kv(&mut body, "qwen3.embedding_length", 2560);
        push_u32_kv(&mut body, "qwen3.attention.head_count", 32);
        push_u32_kv(&mut body, "qwen3.attention.head_count_kv", 8);
        push_u32_kv(&mut body, "qwen3.attention.key_length", 128);
        push_u32_kv(&mut body, "qwen3.context_length", 262144);

        // 8 keys: architecture, tokenizer.ggml.tokens, block_count,
        // embedding_length, head_count, head_count_kv, key_length,
        // context_length.
        let f = write_temp_gguf(8, &body);
        let header = read_header(f.path()).unwrap();
        assert_eq!(header.block_count, 36);
        assert_eq!(header.context_length, 262144);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"NOPE0000").unwrap();
        f.flush().unwrap();
        assert!(read_header(f.path()).is_err());
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        push_u32(&mut buf, 2); // version 2, unsupported
        push_u64(&mut buf, 0);
        push_u64(&mut buf, 0);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&buf).unwrap();
        f.flush().unwrap();
        assert!(read_header(f.path()).is_err());
    }

    #[test]
    fn rejects_missing_architecture() {
        let f = write_temp_gguf(0, &[]);
        let err = read_header(f.path()).unwrap_err();
        assert!(err.contains("general.architecture"));
    }

    #[test]
    fn rejects_a_corrupted_string_length_of_u64_max_without_panicking() {
        // A single flipped bit near u64::MAX in a string's length prefix
        // must not reach `vec![0u8; n]` at all — this must return `Err`,
        // not panic with a capacity overflow or abort in the allocator.
        let mut body = Vec::new();
        push_u64(&mut body, "general.architecture".len() as u64);
        body.extend_from_slice(b"general.architecture");
        push_u32(&mut body, 8); // ValueType::String
        push_u64(&mut body, u64::MAX); // corrupted length — no bytes follow

        let f = write_temp_gguf(1, &body);
        let err = read_header(f.path()).unwrap_err();
        assert!(
            err.contains("sanity ceiling") || err.contains("bytes remain"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_an_array_count_that_could_not_possibly_fit_in_the_file() {
        // The declared count (1 billion u32s = ~4 GB) vastly exceeds this
        // handful-of-bytes file — must error immediately, not spin through
        // the count in a loop before eventually hitting real EOF.
        let mut body = Vec::new();
        push_string_kv(&mut body, "general.architecture", "qwen3");
        push_u64(&mut body, "tokenizer.ggml.tokens".len() as u64);
        body.extend_from_slice(b"tokenizer.ggml.tokens");
        push_u32(&mut body, 9); // ValueType::Array
        push_u32(&mut body, 4); // element type: U32
        push_u64(&mut body, 1_000_000_000); // declared count

        let f = write_temp_gguf(2, &body);
        let err = read_header(f.path()).unwrap_err();
        assert!(err.contains("bytes remain"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_a_file_truncated_mid_value() {
        let mut body = Vec::new();
        push_u64(&mut body, "qwen3.block_count".len() as u64);
        body.extend_from_slice(b"qwen3.block_count");
        push_u32(&mut body, 4); // ValueType::U32
                                // Missing: the 4-byte u32 value itself — the file ends here.

        let f = write_temp_gguf(1, &body);
        assert!(read_header(f.path()).is_err());
    }
}
