//! BrowseComp encrypted-CSV loader.
//!
//! Upstream format (`browse_comp_test_set.csv` at OpenAI blob storage):
//!   columns: `problem, answer, problem_topic, canary`
//!   `problem` + `answer` are base64-encoded XOR ciphertexts.
//!   The per-row XOR key is derived from the `canary` field (per
//!   `openai/simple-evals/browsecomp_eval.py`):
//!     `key = SHA256(canary) * ceil(N / 32)` truncated to N bytes,
//!     where N = length of the encrypted plaintext bytes.
//!
//! See `eval/README-DATASETS.md` for the encryption rationale and the
//! Python reference implementation.
//!
//! The CSV parser is hand-rolled — BrowseComp rows only contain base64
//! tokens and UUID-like canaries (no commas/newlines inside fields), so a
//! lightweight parser handling quoted fields suffices. Avoids pulling in
//! the full `csv` crate (offline-unfriendly dep).

use crate::{Assertion, EvalError, EvalResult, NevoFluxMode, Task};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::Path;

pub fn load(path: &Path) -> EvalResult<Vec<Task>> {
    let body = std::fs::read_to_string(path).map_err(EvalError::Io)?;
    let mut lines = body.lines();
    let header_line = lines.next().ok_or_else(|| EvalError::TaskParse {
        path: path.display().to_string(),
        reason: "csv: empty file".into(),
    })?;
    let headers = parse_csv_line(header_line);

    let problem_idx =
        headers
            .iter()
            .position(|h| h == "problem")
            .ok_or_else(|| EvalError::TaskParse {
                path: path.display().to_string(),
                reason: "missing 'problem' column".into(),
            })?;
    let answer_idx =
        headers
            .iter()
            .position(|h| h == "answer")
            .ok_or_else(|| EvalError::TaskParse {
                path: path.display().to_string(),
                reason: "missing 'answer' column".into(),
            })?;
    let canary_idx =
        headers
            .iter()
            .position(|h| h == "canary")
            .ok_or_else(|| EvalError::TaskParse {
                path: path.display().to_string(),
                reason: "missing 'canary' column".into(),
            })?;

    let mut tasks = Vec::new();
    for (row_idx, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cols = parse_csv_line(line);
        let problem_enc = cols.get(problem_idx).map(String::as_str).unwrap_or("");
        let answer_enc = cols.get(answer_idx).map(String::as_str).unwrap_or("");
        let canary = cols.get(canary_idx).map(String::as_str).unwrap_or("");
        let question = decrypt(problem_enc, canary).map_err(|e| EvalError::TaskParse {
            path: path.display().to_string(),
            reason: format!("row {row_idx} problem decrypt: {e}"),
        })?;
        let answer = decrypt(answer_enc, canary).map_err(|e| EvalError::TaskParse {
            path: path.display().to_string(),
            reason: format!("row {row_idx} answer decrypt: {e}"),
        })?;
        tasks.push(Task {
            id: format!("bc-{:04}", row_idx + 1),
            category: "browsecomp".into(),
            mode: NevoFluxMode::Agent,
            prompt: format!("{question}\n\nReply with just the short answer (1-5 words)."),
            setup: vec![],
            reference: Some(answer.clone()),
            assertions: vec![Assertion::ContainsAny {
                targets: vec![answer],
            }],
            requires_browser: false,
            metadata: Default::default(),
            supports_platform: vec![],
        });
    }
    Ok(tasks)
}

/// Minimal RFC-4180-ish CSV line parser: handles `"..."` quoted fields,
/// `""` as literal quote, comma separators, no embedded newlines. Sufficient
/// for BrowseComp's flat single-line rows.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, in_quotes) {
            ('"', true) => {
                if matches!(chars.peek(), Some('"')) {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            }
            ('"', false) => in_quotes = true,
            (',', false) => {
                out.push(std::mem::take(&mut cur));
            }
            (c, _) => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn derive_key(password: &str, length: usize) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    let key = hasher.finalize().to_vec();
    let mut out = Vec::with_capacity(length);
    while out.len() < length {
        let remaining = length - out.len();
        out.extend_from_slice(&key[..remaining.min(key.len())]);
    }
    out
}

pub fn decrypt(ciphertext_b64: &str, password: &str) -> Result<String, String> {
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(ciphertext_b64)
        .map_err(|e| format!("b64: {e}"))?;
    let key = derive_key(password, encrypted.len());
    let decrypted: Vec<u8> = encrypted
        .iter()
        .zip(key.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    String::from_utf8(decrypted).map_err(|e| format!("utf8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_key_repeats_sha256() {
        // SHA-256 produces a 32-byte digest. derive_key("test", 70) should
        // be [d0..d31][d0..d31][d0..d5] — 70 bytes total, repeating.
        let key70 = derive_key("test", 70);
        assert_eq!(key70.len(), 70);
        let mut h = Sha256::new();
        h.update("test".as_bytes());
        let raw = h.finalize().to_vec();
        assert_eq!(key70[0..32], raw[..]);
        assert_eq!(key70[32..64], raw[..]);
        assert_eq!(key70[64..70], raw[..6]);
    }

    #[test]
    fn decrypt_roundtrip() {
        let plaintext = "hello, world";
        let password = "canary-pwd";
        let key = derive_key(password, plaintext.len());
        let encrypted: Vec<u8> = plaintext
            .as_bytes()
            .iter()
            .zip(key.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&encrypted);
        let recovered = decrypt(&b64, password).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn parse_csv_line_handles_simple_row() {
        let cols = parse_csv_line("a,b,c");
        assert_eq!(cols, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_csv_line_handles_quoted_field() {
        let cols = parse_csv_line(r#""hello, world",b,"c with ""quote"""#);
        assert_eq!(cols, vec![r#"hello, world"#, "b", r#"c with "quote""#]);
    }

    #[test]
    fn load_csv_with_fake_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let plaintext_q = "What is 1+1?";
        let plaintext_a = "2";
        let password = "test-canary";
        let key_q = derive_key(password, plaintext_q.len());
        let key_a = derive_key(password, plaintext_a.len());
        let enc_q: Vec<u8> = plaintext_q
            .as_bytes()
            .iter()
            .zip(key_q.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        let enc_a: Vec<u8> = plaintext_a
            .as_bytes()
            .iter()
            .zip(key_a.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        let b64_q = base64::engine::general_purpose::STANDARD.encode(&enc_q);
        let b64_a = base64::engine::general_purpose::STANDARD.encode(&enc_a);

        let csv_body =
            format!("problem,answer,problem_topic,canary\n{b64_q},{b64_a},math,{password}\n");
        std::fs::write(tmp.path(), csv_body).unwrap();

        let tasks = load(tmp.path()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].prompt.contains("What is 1+1?"));
        assert_eq!(tasks[0].reference, Some("2".into()));
    }
}
