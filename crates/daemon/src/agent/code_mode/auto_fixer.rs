//! MontyAutoFixer - Mechanical code transforms for Monty compatibility.
//! Strips imports, decorators, typing annotations, and type:ignore comments.
//!
//! This is Layer 2 of the four-layer constraint pipeline. It applies
//! deterministic text transforms to fix common violations automatically,
//! with zero LLM cost and sub-millisecond execution.

/// Applies mechanical text transforms to Python code before it reaches
/// the Monty interpreter. All transforms are deterministic and preserve
/// the semantic meaning of code that Monty supports.
pub struct MontyAutoFixer;

impl MontyAutoFixer {
    /// Apply all mechanical transforms to the given Python code.
    ///
    /// Transforms applied:
    /// 1. Strip import statements (`import X` and `from X import Y`)
    /// 2. Strip decorator lines (`@decorator`)
    /// 3. Strip `# type: ignore` comment suffixes
    /// 4. Collapse excessive leading blank lines from stripped content
    pub fn fix(code: &str) -> String {
        let mut result_lines: Vec<String> = Vec::new();

        for line in code.lines() {
            let trimmed = line.trim();

            // Strip import statements: `import X` and `from X import Y`
            if trimmed.starts_with("import ") {
                continue;
            }
            if trimmed.starts_with("from ") && trimmed.contains(" import ") {
                continue;
            }

            // Strip decorator lines: lines starting with `@`
            if trimmed.starts_with('@') {
                continue;
            }

            // Strip `# type: ignore` suffixes (with optional bracket annotations)
            if let Some(pos) = line.find("# type: ignore") {
                let before = line[..pos].trim_end();
                if before.is_empty() {
                    // The entire line is just a type: ignore comment, skip it
                    continue;
                }
                result_lines.push(before.to_string());
                continue;
            }

            result_lines.push(line.to_string());
        }

        // Remove leading blank lines that result from stripping
        let start = result_lines
            .iter()
            .position(|l| !l.trim().is_empty())
            .unwrap_or(result_lines.len());
        let trimmed_lines = &result_lines[start..];

        trimmed_lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_imports() {
        let code = "import os\nimport sys\nx = 1\nfrom pathlib import Path\ny = 2";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "x = 1\ny = 2");
    }

    #[test]
    fn test_strip_decorators() {
        let code = "@dataclass\ndef make_item(name):\n    return {\"name\": name}";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "def make_item(name):\n    return {\"name\": name}");
    }

    #[test]
    fn test_strip_typing_imports() {
        let code = "from typing import List, Dict\nx: List[int] = [1, 2]";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "x: List[int] = [1, 2]");
    }

    #[test]
    fn test_strip_type_ignore() {
        let code = "x = foo()  # type: ignore\ny = bar()  # type: ignore[no-return]";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "x = foo()\ny = bar()");
    }

    #[test]
    fn test_preserves_normal_code() {
        let code = "def greet(name):\n    return f\"Hello, {name}!\"";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, code);
    }

    #[test]
    fn test_strip_multiple_decorators() {
        let code = "@property\n@staticmethod\ndef foo():\n    pass";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "def foo():\n    pass");
    }

    #[test]
    fn test_decorator_with_arguments() {
        let code = "@app.route(\"/\")\ndef index():\n    return \"hello\"";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "def index():\n    return \"hello\"");
    }

    #[test]
    fn test_mixed_code() {
        let code = "import os\nfrom typing import List\n\n@dataclass\ndef process(items):\n    x = compute()  # type: ignore\n    return x";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(
            fixed,
            "def process(items):\n    x = compute()\n    return x"
        );
    }

    #[test]
    fn test_empty_input() {
        let fixed = MontyAutoFixer::fix("");
        assert_eq!(fixed, "");
    }

    #[test]
    fn test_only_imports() {
        let code = "import os\nimport sys\nfrom pathlib import Path";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, "");
    }

    #[test]
    fn test_preserves_indentation() {
        let code = "def foo():\n    if True:\n        x = 1\n        y = 2";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, code);
    }

    #[test]
    fn test_from_variable_not_stripped() {
        // `from_email = ...` should NOT be stripped (no ` import ` present)
        let code = "from_email = \"test@example.com\"\nresult = send(from_email)";
        let fixed = MontyAutoFixer::fix(code);
        assert_eq!(fixed, code);
    }
}
