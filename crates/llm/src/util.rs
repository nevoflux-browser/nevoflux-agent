/// Create a `tokio::process::Command` that correctly resolves CLI tools on all
/// platforms.
///
/// On Windows, npm/node-installed CLIs are `.ps1`/`.cmd` scripts that Rust's
/// `Command::new("name")` cannot find (it only resolves `.exe`).  We locate the
/// actual `.cmd` wrapper on PATH and invoke it via `cmd.exe /C <full-path>` so
/// that stdin/stdout piping works correctly.
pub(crate) fn cli_command(program: &str) -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        // Resolve the real path (e.g. "claude" → "C:\…\claude.cmd")
        let resolved = resolve_windows_cmd(program);
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(&resolved);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        tokio::process::Command::new(program)
    }
}

/// Synchronous version of [`cli_command`].
pub(crate) fn cli_command_sync(program: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        let resolved = resolve_windows_cmd(program);
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(&resolved);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(program)
    }
}

/// On Windows, search PATH for `<program>.cmd` (npm wrapper scripts).
/// Falls back to the bare program name if nothing is found.
#[cfg(target_os = "windows")]
fn resolve_windows_cmd(program: &str) -> String {
    // If it already has an extension or is a full path, use as-is
    if program.contains('.') || program.contains('\\') || program.contains('/') {
        return program.to_string();
    }

    // Search PATH for .cmd / .exe variants
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            // Prefer .cmd (npm scripts) first
            let cmd_path = dir.join(format!("{}.cmd", program));
            if cmd_path.is_file() {
                return cmd_path.to_string_lossy().into_owned();
            }
            let exe_path = dir.join(format!("{}.exe", program));
            if exe_path.is_file() {
                return exe_path.to_string_lossy().into_owned();
            }
        }
    }

    // Fallback: let cmd.exe try to resolve it
    program.to_string()
}
