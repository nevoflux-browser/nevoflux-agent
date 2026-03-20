/// Create a `tokio::process::Command` that correctly resolves CLI tools on all
/// platforms.
///
/// On Windows, npm/node-installed CLIs are `.cmd` scripts that
/// `Command::new("name")` cannot find (it only resolves `.exe` via PATH).
/// We resolve the full path (including `.cmd` extension) so that
/// `CreateProcessW` can launch it directly — no `cmd.exe /C` wrapper needed,
/// which avoids stdin/stdout piping issues.
pub(crate) fn cli_command(program: &str) -> tokio::process::Command {
    let resolved = resolve_program(program);
    tokio::process::Command::new(resolved)
}

/// Resolve the program to a full path on Windows; pass-through on other
/// platforms.
///
/// On Windows, searches PATH for `<program>.cmd` (npm wrapper scripts) and
/// `<program>.exe`, returning the first match.  Falls back to the bare name
/// if nothing is found (the OS will produce its own "not found" error).
#[cfg(target_os = "windows")]
fn resolve_program(program: &str) -> String {
    // Already has an extension or is a full path — use as-is
    if program.contains('.') || program.contains('\\') || program.contains('/') {
        return program.to_string();
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            // Prefer .cmd (npm scripts) — most CLI tools installed via npm
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

#[cfg(not(target_os = "windows"))]
fn resolve_program(program: &str) -> String {
    program.to_string()
}
