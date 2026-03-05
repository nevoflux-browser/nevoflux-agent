//! Lenient JSON parsing utilities for LLM-generated tool arguments.
//!
//! LLMs sometimes produce malformed JSON strings in tool call arguments:
//! - Invalid escape sequences: `\d`, `\w`, `\s` from regex patterns
//! - Unescaped quotes inside string values: `print("=" * 80)`
//!
//! This module provides a three-tier fallback chain:
//! 1. Standard `serde_json::from_str`
//! 2. Fix invalid escape sequences, retry
//! 3. Lenient field extraction using structural boundary detection

/// Parse a JSON string of tool arguments with fallback recovery.
///
/// Attempts standard JSON parsing first, then fixes invalid escape sequences,
/// and finally tries lenient field extraction for malformed strings with
/// unescaped quotes.
pub fn parse_tool_arguments_json(s: &str) -> serde_json::Value {
    if let Ok(v) = serde_json::from_str(s) {
        return v;
    }

    let fixed = fix_invalid_json_escapes(s);
    if let Ok(v) = serde_json::from_str(&fixed) {
        return v;
    }

    // Last resort: LLMs may produce unescaped quotes inside string values.
    // Try lenient field extraction for the "code" field (python-exec).
    lenient_extract_json_field(s, "code").unwrap_or_else(|| serde_json::json!({}))
}

/// Fix invalid JSON escape sequences produced by LLMs.
///
/// LLMs sometimes emit JSON strings containing raw regex patterns like `\d`, `\w`, `\s`
/// which are not valid JSON escapes. This function escapes lone backslashes that are not
/// followed by a valid JSON escape character.
pub fn fix_invalid_json_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 32);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next)
                    if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') =>
                {
                    result.push('\\');
                    result.push(next);
                }
                Some(next) => {
                    // Invalid escape like \d, \w, \s — double the backslash
                    result.push('\\');
                    result.push('\\');
                    result.push(next);
                }
                None => {
                    result.push('\\');
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Leniently unescape a JSON string value.
///
/// Handles standard JSON escapes (`\"`, `\\`, `\n`, etc.) and treats unknown escape
/// sequences (like `\d`, `\w`) as literal backslash + char. Bare (unescaped) quotes
/// are passed through as-is — this is used after boundary detection has already
/// identified the string extent.
fn unescape_json_string_lenient(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('/') => result.push('/'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('b') => result.push('\u{0008}'),
                Some('f') => result.push('\u{000C}'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(cp) {
                            result.push(ch);
                        }
                    }
                }
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Extract a string field from a malformed JSON object string.
///
/// Last-resort fallback when standard JSON parsing fails due to unescaped quotes
/// inside string values. Finds the field by key name, then determines the value
/// boundaries using structural cues (last `"}` in the string).
///
/// Only reliable for single-field JSON objects like `{"code": "..."}`.
fn lenient_extract_json_field(json_str: &str, field_name: &str) -> Option<serde_json::Value> {
    let trimmed = json_str.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return None;
    }

    // Find "field_name" followed by :
    let key_pattern = format!("\"{}\"", field_name);
    let key_pos = trimmed.find(&key_pattern)?;
    let after_key = trimmed[key_pos + key_pattern.len()..].trim_start();
    if !after_key.starts_with(':') {
        return None;
    }
    let after_colon = after_key[1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }

    // Value content starts after the opening quote
    let value_start = trimmed.len() - after_colon.len() + 1;

    // Find end: scan backward from end of string.
    // Skip closing }, optional whitespace, then expect closing "
    let bytes = trimmed.as_bytes();
    let mut pos = trimmed.len();
    while pos > value_start && bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    if pos <= value_start || bytes[pos - 1] != b'}' {
        return None;
    }
    pos -= 1;
    while pos > value_start && bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    if pos <= value_start || bytes[pos - 1] != b'"' {
        return None;
    }
    let value_end = pos - 1;

    let raw = if value_end > value_start {
        &trimmed[value_start..value_end]
    } else {
        ""
    };

    let value = unescape_json_string_lenient(raw);
    let mut map = serde_json::Map::new();
    map.insert(field_name.to_string(), serde_json::Value::String(value));
    Some(serde_json::Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_invalid_json_escapes() {
        // Valid escapes should pass through
        let input = r#"{"code": "hello\nworld"}"#;
        assert_eq!(fix_invalid_json_escapes(input), input);

        // Invalid escapes like \d should be double-escaped
        let input = r#"{"code": "re.match(\\d+)"}"#;
        let fixed = fix_invalid_json_escapes(input);
        assert!(fixed.contains(r"\\d"));
    }

    #[test]
    fn test_unescape_json_string_lenient() {
        // Standard escapes
        assert_eq!(
            unescape_json_string_lenient(r#"hello\nworld"#),
            "hello\nworld"
        );
        assert_eq!(unescape_json_string_lenient(r#"a\"b"#), "a\"b");
        assert_eq!(unescape_json_string_lenient(r#"a\\b"#), "a\\b");

        // Unknown escapes preserved as literal
        assert_eq!(unescape_json_string_lenient(r#"\d+"#), r#"\d+"#);

        // Bare quotes pass through
        assert_eq!(
            unescape_json_string_lenient(r#"print("hi")"#),
            r#"print("hi")"#
        );

        // Unicode escape
        assert_eq!(unescape_json_string_lenient(r#"\u0041"#), "A");

        // Mixed: escaped quote + bare quote
        assert_eq!(
            unescape_json_string_lenient(r#"print(\"=" * 80)"#),
            r#"print("=" * 80)"#
        );
    }

    #[test]
    fn test_lenient_extract_json_field_basic() {
        let json = r#"{"code": "print(1)"}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        assert_eq!(result["code"].as_str().unwrap(), "print(1)");
    }

    #[test]
    fn test_lenient_extract_json_field_unescaped_quotes() {
        // Malformed JSON with unescaped quotes
        let json = r#"{"code": "print(\"=" * 80)"}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        assert_eq!(result["code"].as_str().unwrap(), r#"print("=" * 80)"#);
    }

    #[test]
    fn test_lenient_extract_json_field_empty_code() {
        let json = r#"{"code": ""}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        assert_eq!(result["code"].as_str().unwrap(), "");
    }

    #[test]
    fn test_lenient_extract_json_field_with_newlines() {
        let json = r#"{"code": "line1\nline2\nline3"}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        assert_eq!(result["code"].as_str().unwrap(), "line1\nline2\nline3");
    }

    #[test]
    fn test_lenient_extract_json_field_missing_field() {
        let json = r#"{"text": "hello"}"#;
        assert!(lenient_extract_json_field(json, "code").is_none());
    }

    #[test]
    fn test_lenient_extract_json_field_not_json() {
        assert!(lenient_extract_json_field("not json", "code").is_none());
        assert!(lenient_extract_json_field("", "code").is_none());
    }

    #[test]
    fn test_lenient_extract_json_field_complex_code() {
        let json = r#"{"code": "d = {\"key\": \"value\"}\nprint(d)"}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        assert_eq!(
            result["code"].as_str().unwrap(),
            "d = {\"key\": \"value\"}\nprint(d)"
        );
    }

    #[test]
    fn test_parse_tool_arguments_json_valid() {
        let result = parse_tool_arguments_json(r#"{"code": "print(1)"}"#);
        assert_eq!(result["code"].as_str().unwrap(), "print(1)");
    }

    #[test]
    fn test_parse_tool_arguments_json_invalid_escapes() {
        let result = parse_tool_arguments_json(r#"{"code": "re.match(\\d+, s)"}"#);
        assert!(result["code"].as_str().is_some());
    }

    #[test]
    fn test_parse_tool_arguments_json_unescaped_quotes() {
        let malformed = r#"{"code": "print(\"=" * 80)"}"#;
        let result = parse_tool_arguments_json(malformed);
        assert_eq!(result["code"].as_str().unwrap(), r#"print("=" * 80)"#);
    }

    #[test]
    fn test_lenient_extract_json_field_bare_quotes() {
        let json = r#"{"code": "print(\"Title\")\nprint(\"=" * 80)\nprint(\"Done\")"}"#;
        let result = lenient_extract_json_field(json, "code").unwrap();
        let code = result["code"].as_str().unwrap();
        assert!(code.contains("print(\"Title\")"));
        assert!(code.contains("print(\"=\" * 80)"));
    }
}
