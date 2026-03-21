/// Create a `tokio::process::Command` for a CLI tool, resolving the full path
/// on all platforms.
///
/// On Windows, npm-installed CLIs are `.cmd` wrapper scripts. The `which` crate
/// resolves them correctly via PATHEXT. Rust's `Command::new()` handles `.cmd`
/// files internally by invoking `cmd.exe`.
///
/// This approach matches Goose's implementation:
/// `SearchPaths::builder().with_npm().resolve(command)` → `Command::new(resolved)`
pub(crate) fn cli_command(program: &str) -> tokio::process::Command {
    let resolved = resolve_program(program);
    tokio::process::Command::new(resolved)
}

/// Resolve a program name to its full executable path.
///
/// Uses the `which` crate which correctly handles Windows PATHEXT
/// (finding `.cmd`, `.bat`, `.exe` etc.) and Unix PATH lookup.
fn resolve_program(program: &str) -> std::path::PathBuf {
    // If it already has a path separator, use as-is
    if program.contains(std::path::MAIN_SEPARATOR)
        || program.contains('/')
        || program.contains('.')
    {
        return std::path::PathBuf::from(program);
    }

    // Build extended search paths (include npm global dirs like Goose does)
    let search_path = build_search_path();
    which::which_in(program, search_path.as_deref(), std::env::current_dir().unwrap_or_default())
        .unwrap_or_else(|_| std::path::PathBuf::from(program))
}

/// Build an extended PATH that includes npm global directories.
///
/// Mirrors Goose's `SearchPaths::builder().with_npm()`:
/// - Windows: adds `%APPDATA%/npm`
/// - Unix: adds `~/.npm-global/bin`
fn build_search_path() -> Option<std::ffi::OsString> {
    let mut extra_dirs: Vec<std::path::PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        // npm global bin on Windows: %APPDATA%\npm
        if let Some(appdata) = std::env::var_os("APPDATA") {
            extra_dirs.push(std::path::PathBuf::from(appdata).join("npm"));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            extra_dirs.push(std::path::PathBuf::from(&home).join(".npm-global/bin"));
            extra_dirs.push(std::path::PathBuf::from(&home).join(".local/bin"));
        }
        extra_dirs.push(std::path::PathBuf::from("/usr/local/bin"));
    }

    // Prepend extra dirs to existing PATH
    let existing = std::env::var_os("PATH");
    let all_paths = extra_dirs
        .into_iter()
        .chain(
            existing
                .as_ref()
                .map(std::env::split_paths)
                .into_iter()
                .flatten(),
        );

    std::env::join_paths(all_paths).ok()
}
