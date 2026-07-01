//! Automation policy: the default browser-centric envelope + per-task opt-ins.
//!
//! The default (`browser_only`) excludes shell, filesystem writes, and uploads.
//! Tasks opt into each dangerous class explicitly; the `SENSITIVE_PATHS` floor
//! and the fs sandbox (applied elsewhere) still bound even opted-in access.

const SHELL_TOOLS: &[&str] = &["run_command", "bash"];
const FS_WRITE_TOOLS: &[&str] = &["write_file", "write", "edit"];
const UPLOAD_TOOLS: &[&str] = &["uploadFile"];

/// Per-task capability policy.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Admit shell tools (`run_command`, `bash`).
    pub allow_shell: bool,
    /// Admit filesystem-write tools (`write_file`, `write`, `edit`).
    pub allow_fs_write: bool,
    /// Admit `uploadFile`.
    pub allow_upload: bool,
    /// If non-empty, restrict `navigate`/`web_fetch` to these domains (checked elsewhere).
    pub domain_allowlist: Vec<String>,
    /// Retry even after a mutating tool ran (caller asserts idempotency).
    pub idempotent: bool,
    /// Disable auto-retry entirely.
    pub no_retry: bool,
}

impl Policy {
    /// Default envelope: browser tools only; shell/fs/upload excluded.
    pub fn browser_only() -> Self {
        Self {
            allow_shell: false,
            allow_fs_write: false,
            allow_upload: false,
            domain_allowlist: Vec::new(),
            idempotent: false,
            no_retry: false,
        }
    }

    /// Intersect the mode's tool catalog with what this policy admits.
    pub fn tool_allowlist(&self, mode_tools: &[String]) -> Vec<String> {
        mode_tools
            .iter()
            .filter(|t| {
                let t = t.as_str();
                if SHELL_TOOLS.contains(&t) {
                    return self.allow_shell;
                }
                if FS_WRITE_TOOLS.contains(&t) {
                    return self.allow_fs_write;
                }
                if UPLOAD_TOOLS.contains(&t) {
                    return self.allow_upload;
                }
                true
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_only_policy_excludes_dangerous_tools() {
        let p = Policy::browser_only();
        let mode = [
            "navigate",
            "click",
            "get_content",
            "run_command",
            "write_file",
            "uploadFile",
            "bash",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        let allow = p.tool_allowlist(&mode);
        assert!(allow.contains(&"navigate".to_string()));
        assert!(allow.contains(&"click".to_string()));
        assert!(!allow.contains(&"run_command".to_string()));
        assert!(!allow.contains(&"write_file".to_string()));
        assert!(!allow.contains(&"uploadFile".to_string()));
        assert!(!allow.contains(&"bash".to_string()));
    }

    #[test]
    fn opt_in_shell_admits_shell_tools() {
        let mut p = Policy::browser_only();
        p.allow_shell = true;
        let mode = ["navigate", "run_command", "bash"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let allow = p.tool_allowlist(&mode);
        assert!(allow.contains(&"run_command".to_string()));
        assert!(allow.contains(&"bash".to_string()));
    }
}
