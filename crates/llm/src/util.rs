/// Create a `tokio::process::Command` that correctly resolves CLI tools on all
/// platforms.
///
/// On Windows, npm/node-installed CLIs are `.cmd` scripts.
/// `Command::new("name")` cannot find them (only resolves `.exe` via PATH),
/// and even with a full path, `.cmd` files cannot be executed directly by
/// `CreateProcessW` ("batch file arguments are invalid").
///
/// Solution: resolve the full path, then wrap `.cmd` files with `cmd.exe /C`.
pub(crate) fn cli_command(program: &str) -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        let resolved = resolve_windows_program(program);
        if resolved.to_lowercase().ends_with(".cmd")
            || resolved.to_lowercase().ends_with(".bat")
        {
            let mut cmd = tokio::process::Command::new("cmd.exe");
            cmd.arg("/C").arg(&resolved);
            cmd
        } else {
            tokio::process::Command::new(resolved)
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        tokio::process::Command::new(program)
    }
}

/// Resolve a program name to its full path on Windows.
///
/// Searches PATH for `<program>.cmd` (npm wrapper scripts) and
/// `<program>.exe`, returning the first match.  Falls back to the bare name
/// if nothing is found.
#[cfg(target_os = "windows")]
fn resolve_windows_program(program: &str) -> String {
    // Already has an extension or is a full path — use as-is
    if program.contains('.') || program.contains('\\') || program.contains('/') {
        return program.to_string();
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
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

    program.to_string()
}
