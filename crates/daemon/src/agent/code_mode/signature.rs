//! Tool signature generation for Code Mode orchestrate tool.
//!
//! Generates Python function signatures from ToolDefinition JSON Schema,
//! used in the orchestrate tool description (compact) and Monty type checker (full).

use serde_json::Value;

/// Maps a JSON Schema type definition to a Python type annotation string.
///
/// Handles primitive types, arrays with typed items, bare objects, enums,
/// `anyOf` unions (including nullable types), and falls back to `Any` for
/// unrecognized or empty schemas.
pub fn json_schema_to_python_type(schema: &Value) -> String {
    // Enum takes priority — even if "type" is also present
    if let Some(enum_values) = schema.get("enum") {
        if let Some(arr) = enum_values.as_array() {
            let literals: Vec<String> = arr
                .iter()
                .map(|v| match v {
                    Value::String(s) => format!("\"{s}\""),
                    other => other.to_string(),
                })
                .collect();
            return format!("Literal[{}]", literals.join(", "));
        }
    }

    // anyOf union types
    if let Some(any_of) = schema.get("anyOf") {
        if let Some(variants) = any_of.as_array() {
            let types: Vec<String> = variants
                .iter()
                .map(|v| json_schema_to_python_type(v))
                .collect();
            return types.join(" | ");
        }
    }

    // Type-based mapping
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "float".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("null") => "None".to_string(),
        Some("array") => {
            let item_type = schema
                .get("items")
                .map(|items| json_schema_to_python_type(items))
                .unwrap_or_else(|| "Any".to_string());
            format!("list[{item_type}]")
        }
        Some("object") => "dict[str, Any]".to_string(),
        _ => "Any".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_primitive_types() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "string"})),
            "str"
        );
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "integer"})),
            "int"
        );
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "number"})),
            "float"
        );
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "boolean"})),
            "bool"
        );
    }

    #[test]
    fn test_null_type() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "null"})),
            "None"
        );
    }

    #[test]
    fn test_array_type() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "array", "items": {"type": "string"}})),
            "list[str]"
        );
    }

    #[test]
    fn test_array_without_items() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "array"})),
            "list[Any]"
        );
    }

    #[test]
    fn test_bare_object() {
        assert_eq!(
            json_schema_to_python_type(&json!({"type": "object"})),
            "dict[str, Any]"
        );
    }

    #[test]
    fn test_object_with_properties() {
        assert_eq!(
            json_schema_to_python_type(
                &json!({"type": "object", "properties": {"name": {"type": "string"}}})
            ),
            "dict[str, Any]"
        );
    }

    #[test]
    fn test_enum_type() {
        assert_eq!(
            json_schema_to_python_type(
                &json!({"type": "string", "enum": ["left", "right", "middle"]})
            ),
            "Literal[\"left\", \"right\", \"middle\"]"
        );
    }

    #[test]
    fn test_any_of() {
        assert_eq!(
            json_schema_to_python_type(
                &json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})
            ),
            "str | int"
        );
    }

    #[test]
    fn test_nullable() {
        assert_eq!(
            json_schema_to_python_type(
                &json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
            ),
            "str | None"
        );
    }

    #[test]
    fn test_no_type() {
        assert_eq!(json_schema_to_python_type(&json!({})), "Any");
    }

    #[test]
    fn test_nested_array() {
        assert_eq!(
            json_schema_to_python_type(
                &json!({"type": "array", "items": {"type": "array", "items": {"type": "integer"}}})
            ),
            "list[list[int]]"
        );
    }
}
