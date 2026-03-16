//! Command resolution utilities for MCP server process spawning.
//!
//! Handles splitting command strings and resolving executable paths,
//! which is needed when MCP configs put the full command line in a
//! single string (e.g. `"npx -y @upstash/context7-mcp"`).

/// Split a command string into the executable and its arguments.
///
/// If `args` is empty and `command` contains whitespace, the command string
/// is split on whitespace. Otherwise the command and args are returned as-is.
///
/// # Examples
///
/// ```
/// use nevoflux_mcp::command::split_command;
///
/// // Full command in one string, no separate args
/// let (cmd, args) = split_command("npx -y @upstash/context7-mcp", &[]);
/// assert!(cmd.contains("npx")); // may be resolved to full path
/// assert_eq!(args, vec!["-y", "@upstash/context7-mcp"]);
///
/// // Already separated
/// let (cmd, args) = split_command("npx", &["-y", "@upstash/context7-mcp"]);
/// assert!(cmd.contains("npx"));
/// assert_eq!(args, vec!["-y", "@upstash/context7-mcp"]);
/// ```
pub fn split_command<'a>(command: &'a str, args: &[&'a str]) -> (String, Vec<String>) {
    if args.is_empty() && command.contains(' ') {
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return (command.to_string(), Vec::new());
        }
        let cmd = resolve_command_path(parts[0]);
        let extra_args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        (cmd, extra_args)
    } else {
        let cmd = resolve_command_path(command);
        (cmd, args.iter().map(|s| s.to_string()).collect())
    }
}

/// Try to resolve a command name to its absolute path.
///
/// When the daemon process runs without a full login shell environment
/// (e.g., started by a browser extension), tools like `npx` installed
/// via nvm won't be on PATH. This function:
///
/// 1. Returns absolute paths as-is
/// 2. Tries the current process PATH via `which`
/// 3. Falls back to `sh -lc 'which <cmd>'` which sources the user's
///    login profile (handles nvm, pyenv, etc.)
/// 4. Returns the original command as last resort
fn resolve_command_path(command: &str) -> String {
    // Absolute or relative paths don't need resolution
    if command.starts_with('/') || command.starts_with('.') {
        return command.to_string();
    }

    // Try `which` with current PATH
    if let Some(path) = which_command(command) {
        return path;
    }

    // Fall back to login shell which sources ~/.bashrc, ~/.zshrc, nvm, etc.
    #[cfg(unix)]
    if let Some(path) = which_via_login_shell(command) {
        return path;
    }

    // On Windows, try common locations for npx/node
    #[cfg(windows)]
    if let Some(path) = which_windows_fallback(command) {
        return path;
    }

    tracing::warn!(
        command = %command,
        "Could not resolve command path, using as-is"
    );
    command.to_string()
}

/// Run `which <command>` using the current process PATH.
fn which_command(command: &str) -> Option<String> {
    #[cfg(unix)]
    let which_cmd = "which";
    #[cfg(windows)]
    let which_cmd = "where";

    let output = std::process::Command::new(which_cmd)
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8(output.stdout).ok()?;
        let path = path.lines().next()?.trim();
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

/// Resolve command via login shell (handles nvm, pyenv, etc.).
#[cfg(unix)]
fn which_via_login_shell(command: &str) -> Option<String> {
    let output = std::process::Command::new("sh")
        .args(["-lc", &format!("which {}", command)])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8(output.stdout).ok()?;
        let path = path.trim();
        if !path.is_empty() {
            tracing::debug!(
                command = %command,
                resolved = %path,
                "Resolved command via login shell"
            );
            return Some(path.to_string());
        }
    }
    None
}

/// Try common Windows paths for node-based tools.
#[cfg(windows)]
fn which_windows_fallback(command: &str) -> Option<String> {
    use std::path::PathBuf;

    // Check common npm/node install locations on Windows
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let npm_path = PathBuf::from(&appdata)
            .join("npm")
            .join(format!("{}.cmd", command));
        if npm_path.exists() {
            return Some(npm_path.to_string_lossy().to_string());
        }
    }

    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let node_path = PathBuf::from(&program_files)
            .join("nodejs")
            .join(format!("{}.cmd", command));
        if node_path.exists() {
            return Some(node_path.to_string_lossy().to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_command_with_spaces() {
        let (cmd, args) = split_command("npx -y @upstash/context7-mcp", &[]);
        // cmd should be "npx" or resolved path to npx
        assert!(cmd.contains("npx") || cmd.ends_with("npx"));
        assert_eq!(args, vec!["-y", "@upstash/context7-mcp"]);
    }

    #[test]
    fn test_split_command_already_separated() {
        let (cmd, args) = split_command("npx", &["-y", "@test/mcp"]);
        assert!(cmd.contains("npx") || cmd.ends_with("npx"));
        assert_eq!(args, vec!["-y", "@test/mcp"]);
    }

    #[test]
    fn test_split_command_no_spaces() {
        let (cmd, args) = split_command("node", &[]);
        assert!(cmd.contains("node"));
        assert!(args.is_empty());
    }

    #[test]
    fn test_split_command_absolute_path() {
        let (cmd, args) = split_command("/usr/bin/node server.js", &[]);
        assert_eq!(cmd, "/usr/bin/node");
        assert_eq!(args, vec!["server.js"]);
    }

    #[test]
    fn test_resolve_command_path_absolute() {
        assert_eq!(
            resolve_command_path("/usr/bin/env"),
            "/usr/bin/env"
        );
    }

    #[test]
    fn test_resolve_command_path_relative() {
        assert_eq!(
            resolve_command_path("./my-server"),
            "./my-server"
        );
    }

    #[test]
    fn test_resolve_command_unknown() {
        // Non-existent command should return as-is
        let result = resolve_command_path("nonexistent_command_12345");
        assert_eq!(result, "nonexistent_command_12345");
    }
}
